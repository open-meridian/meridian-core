//! The change log a store captures in Postgres: every row its transactions
//! write, in the same transaction, numbered from its partition's head row.
//!
//! The instrument store, the street store and the conductor's configuration
//! store each write many tables from many paths. Their log is taken where no
//! path can miss it: a trigger on each of the store's tables appends the row
//! written -- its table, what was done to it and its columns -- to
//! `{prefix}_change` under the partition's next sequence, one sequence per
//! transaction, taken from the head row with the head row's lock, so a
//! rollback takes it back and there are no holes. The same trigger is the
//! fence: a writer names its epoch on its connection (`meridian.{prefix}_epoch`),
//! and a commit under any other epoch than the head row's is refused in its
//! own transaction (SQLSTATE `MF001`). A connection naming none -- the
//! migration Job -- is no writer and is not fenced.
//!
//! The log starts where it was installed: a store that held rows before keeps
//! the state they make as its first sequence, never kept, so a follower
//! asking from the start is told it is behind and reloads the partition at
//! its head (`kept_from`).

use postgres::GenericClient;

use crate::{check_kept, origin, Change, Page, ReadChangesReply, ReadError, CONTROL, FENCED};

/// The SQLSTATE a fenced commit is refused with.
///
/// (Integrity at rest's scrub of a captured log: [`Spec::scrub`].)
pub const FENCED_STATE: &str = "MF001";

/// What a store captures, and where.
#[derive(Debug, Clone, Copy)]
pub struct Spec {
    /// Names the change table (`{prefix}_change`), the trigger function and
    /// the connection's epoch setting.
    pub prefix: &'static str,
    /// The head table, one row per partition, and the partition the store's
    /// changes are numbered in.
    pub head: &'static str,
    pub partition: &'static str,
    /// Every table whose rows are the store's changes.
    pub tables: &'static [&'static str],
    /// The store's tables that are not its changes, each with why: a cache
    /// another source fills again, or working state the record never was.
    /// Named so a test can hold every table to one list or the other.
    pub not_captured: &'static [(&'static str, &'static str)],
}

impl Spec {
    /// Every partition the log keeps: the store's own, and `control`.
    pub fn partitions(&self) -> [&'static str; 2] {
        [self.partition, CONTROL]
    }

    fn change_table(&self) -> String {
        format!("{}_change", self.prefix)
    }

    /// Re-read the log (step 3b, integrity at rest): every row's digest
    /// checked against its body, a row kept before digests given its own, a
    /// row that no longer matches named by its partition's first bad
    /// sequence. Postgres verifies each page's checksum as it reads it.
    pub fn scrub(
        &self,
        client: &mut impl GenericClient,
    ) -> Result<crate::integrity::Scrubbed, postgres::Error> {
        match self.scrub_pages(client) {
            Err(failed) => match crate::integrity::page_finding(
                &failed,
                &format!("a page of {} failed its checksum", self.change_table()),
            ) {
                Some(finding) => Ok(crate::integrity::Scrubbed {
                    findings: vec![finding],
                    ..Default::default()
                }),
                None => Err(failed),
            },
            scrubbed => scrubbed,
        }
    }

    fn scrub_pages(
        &self,
        client: &mut impl GenericClient,
    ) -> Result<crate::integrity::Scrubbed, postgres::Error> {
        let change = self.change_table();
        let filled = client.execute(
            &format!(
                "UPDATE {change} SET digest = sha256(convert_to(body::text, 'UTF8'))
                  WHERE digest IS NULL"
            ),
            &[],
        )?;
        let checked = client
            .query_one(&format!("SELECT count(*) FROM {change}"), &[])?
            .get::<_, i64>(0) as u64;
        let findings = client
            .query(
                &format!(
                    "SELECT partition, min(sequence), min(committed_at_ns) FROM {change}
                      WHERE digest <> sha256(convert_to(body::text, 'UTF8'))
                      GROUP BY partition ORDER BY partition"
                ),
                &[],
            )?
            .iter()
            .map(|row| crate::integrity::Finding {
                partition: row.get(0),
                sequence: row.get::<_, i64>(1) as u64,
                committed_at_ns: row.get(2),
                what: "a row does not match its digest: the record changed beneath the store"
                    .to_string(),
            })
            .collect();
        Ok(crate::integrity::Scrubbed {
            checked,
            filled,
            findings,
        })
    }

    /// The connection setting a writer names its epoch in.
    pub fn epoch_setting(&self) -> String {
        format!("meridian.{}_epoch", self.prefix)
    }

    /// The connection setting a writer names the deployment's time in.
    pub fn now_setting(&self) -> String {
        format!("meridian.{}_now_ns", self.prefix)
    }

    /// The SQL that installs the log: run by the store's `migrate`, after its
    /// numbered migrations, every time; each statement may run again.
    pub fn install_sql(&self) -> String {
        self.install_sql_for(self.tables)
    }

    /// [`Spec::install_sql`] for the captured tables the schema holds.
    fn install_sql_for(&self, tables: &[&str]) -> String {
        let head = self.head;
        let partition = self.partition;
        let change = self.change_table();
        let prefix = self.prefix;
        let epoch_setting = self.epoch_setting();
        let now_setting = self.now_setting();
        // The refusal's words, inside a SQL literal.
        let fenced = FENCED.replace('\'', "''");
        let any_rows = tables
            .iter()
            .map(|table| format!("EXISTS (SELECT 1 FROM {table})"))
            .collect::<Vec<_>>()
            .join(" OR ");
        let mut sql = format!(
            "CREATE TABLE IF NOT EXISTS {head} (
                partition text   PRIMARY KEY,
                sequence  bigint NOT NULL CHECK (sequence >= 0)
            );
            ALTER TABLE {head}
                ADD COLUMN IF NOT EXISTS epoch     bigint  NOT NULL DEFAULT 1,
                ADD COLUMN IF NOT EXISTS origin    text    NOT NULL DEFAULT '',
                ADD COLUMN IF NOT EXISTS kept_from bigint,
                ADD COLUMN IF NOT EXISTS tx        xid8,
                ADD COLUMN IF NOT EXISTS ordinal   integer NOT NULL DEFAULT 0,
                ADD COLUMN IF NOT EXISTS claimed   boolean NOT NULL DEFAULT false;
            -- The database's identity the writer last saw, on the control
            -- row (contract v30, stage C): a new timeline raises the epoch.
            ALTER TABLE {head}
                ADD COLUMN IF NOT EXISTS database_identity text NOT NULL DEFAULT '';
            INSERT INTO {head} (partition, sequence) VALUES ('{partition}', 0), ('{CONTROL}', 0)
                ON CONFLICT (partition) DO NOTHING;
            -- What the store held before its log began is not in it: a
            -- follower with nothing is told to reload, never handed a log
            -- that would leave those rows out. A head already past 0 says
            -- so by itself; at 0, the rows held are numbered 1, never kept.
            UPDATE {head} SET sequence = 1
             WHERE partition = '{partition}' AND kept_from IS NULL AND sequence = 0
               AND ({any_rows});
            UPDATE {head} SET kept_from = sequence + 1 WHERE kept_from IS NULL;
            CREATE TABLE IF NOT EXISTS {change} (
                partition       text    NOT NULL,
                sequence        bigint  NOT NULL CHECK (sequence > 0),
                ordinal         integer NOT NULL CHECK (ordinal >= 0),
                epoch           bigint  NOT NULL,
                -- NULL for a change made here: the partition's own origin.
                origin          text,
                committed_at_ns bigint  NOT NULL,
                kind            text    NOT NULL,
                body            jsonb   NOT NULL,
                PRIMARY KEY (partition, sequence, ordinal)
            );
            -- Each row's digest, SHA-256 of its body as text (step 3b,
            -- integrity at rest): checked when it is read and by the scrub.
            ALTER TABLE {change} ADD COLUMN IF NOT EXISTS digest bytea;
            CREATE TABLE IF NOT EXISTS {prefix}_integrity_finding (
                id              bigserial PRIMARY KEY,
                partition       text   NOT NULL,
                sequence        bigint NOT NULL,
                committed_at_ns bigint NOT NULL,
                what            text   NOT NULL,
                member          text   NOT NULL,
                origin          text   NOT NULL,
                found_at_ns     bigint NOT NULL,
                UNIQUE (partition, sequence, what)
            );
            CREATE TABLE IF NOT EXISTS {prefix}_scrub (
                started_at_ns  bigint PRIMARY KEY,
                finished_at_ns bigint NOT NULL,
                checked        bigint NOT NULL,
                filled         bigint NOT NULL,
                findings       bigint NOT NULL,
                member         text   NOT NULL
            );
            CREATE OR REPLACE FUNCTION {prefix}_capture() RETURNS trigger
                LANGUAGE plpgsql AS $f$
            DECLARE
                h    record;
                mine text := current_setting('{epoch_setting}', true);
                seq  bigint;
                ord  integer;
                body jsonb;
            BEGIN
                SELECT epoch, tx INTO h FROM {head} WHERE partition = '{partition}' FOR UPDATE;
                IF mine IS NOT NULL AND mine <> '' AND mine::bigint <> h.epoch THEN
                    RAISE EXCEPTION USING ERRCODE = '{FENCED_STATE}',
                        MESSAGE = format('{fenced} (this writer''s %s, the partition''s %s)', mine, h.epoch);
                END IF;
                IF h.tx IS DISTINCT FROM pg_current_xact_id() THEN
                    UPDATE {head} SET sequence = sequence + 1, tx = pg_current_xact_id(), ordinal = 0,
                                      claimed = false
                     WHERE partition = '{partition}' RETURNING sequence, ordinal INTO seq, ord;
                ELSE
                    UPDATE {head} SET ordinal = ordinal + 1
                     WHERE partition = '{partition}' RETURNING sequence, ordinal INTO seq, ord;
                END IF;
                IF TG_LEVEL = 'STATEMENT' THEN
                    body := '{{}}'::jsonb;
                ELSIF TG_OP = 'DELETE' THEN
                    body := to_jsonb(OLD);
                ELSE
                    body := to_jsonb(NEW);
                END IF;
                INSERT INTO {change} (partition, sequence, ordinal, epoch, committed_at_ns, kind, body,
                                      digest)
                VALUES ('{partition}', seq, ord, h.epoch,
                        coalesce(nullif(current_setting('{now_setting}', true), '')::bigint, 0),
                        TG_TABLE_NAME || '.' || lower(TG_OP), body,
                        sha256(convert_to(body::text, 'UTF8')));
                -- Heard after commit by the tail's publisher, once a
                -- transaction: Postgres folds a channel's equal payloads.
                PERFORM pg_notify('{prefix}_changes', '');
                RETURN NULL;
            END
            $f$;
            "
        );
        for table in tables {
            sql.push_str(&format!(
                "DROP TRIGGER IF EXISTS {prefix}_captured ON {table};
                CREATE TRIGGER {prefix}_captured AFTER INSERT OR DELETE ON {table}
                    FOR EACH ROW EXECUTE FUNCTION {prefix}_capture();
                DROP TRIGGER IF EXISTS {prefix}_captured_update ON {table};
                CREATE TRIGGER {prefix}_captured_update AFTER UPDATE ON {table}
                    FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*)
                    EXECUTE FUNCTION {prefix}_capture();
                DROP TRIGGER IF EXISTS {prefix}_captured_truncate ON {table};
                CREATE TRIGGER {prefix}_captured_truncate AFTER TRUNCATE ON {table}
                    FOR EACH STATEMENT EXECUTE FUNCTION {prefix}_capture();
                "
            ));
        }
        sql
    }

    /// Install the log; see [`Spec::install_sql`]. Done once per version of
    /// it: a log installed as this one would be is left alone, so a
    /// migration takes no table's lock it does not need. Where writers are
    /// running -- a rolling upgrade's migration Job -- each try waits at most
    /// a few seconds for a table's lock and is tried again, never deadlocking
    /// the store's writers for longer than one of their transactions.
    pub fn install(&self, client: &mut impl GenericClient) -> Result<(), postgres::Error> {
        let present: Vec<String> = client
            .query(
                "SELECT name FROM unnest($1::text[]) AS name WHERE to_regclass(name) IS NOT NULL",
                &[&self
                    .tables
                    .iter()
                    .map(|t| t.to_string())
                    .collect::<Vec<_>>()],
            )?
            .into_iter()
            .map(|row| row.get(0))
            .collect();
        let present: Vec<&str> = present.iter().map(String::as_str).collect();
        let sql = self.install_sql_for(&present);
        let print = fingerprint(&sql);
        if self.installed(client, &print, present.len())? {
            return Ok(());
        }
        let mut attempt = 0u32;
        loop {
            let tried = client.batch_execute(&format!(
                "SET lock_timeout = '3s';
                 {sql}
                 COMMENT ON FUNCTION {prefix}_capture() IS '{print}';
                 RESET lock_timeout;",
                prefix = self.prefix
            ));
            match tried {
                Ok(()) => return Ok(()),
                Err(failed) if attempt < 30 && contended(&failed) => {
                    let _ = client.batch_execute("RESET lock_timeout");
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(100 * u64::from(attempt)));
                }
                Err(failed) => {
                    let _ = client.batch_execute("RESET lock_timeout");
                    return Err(failed);
                }
            }
        }
    }

    /// Whether this version of the log is installed: its function carries
    /// this fingerprint and every captured table its triggers.
    fn installed(
        &self,
        client: &mut impl GenericClient,
        print: &str,
        tables: usize,
    ) -> Result<bool, postgres::Error> {
        let row = client.query_one(
            "SELECT obj_description(to_regprocedure($1), 'pg_proc'),
                    (SELECT count(*) FROM pg_trigger t
                      WHERE t.tgname = $2 AND t.tgrelid = ANY(
                            SELECT to_regclass(name) FROM unnest($3::text[]) AS name))",
            &[
                &format!("{}_capture()", self.prefix),
                &format!("{}_captured", self.prefix),
                &self
                    .tables
                    .iter()
                    .map(|t| t.to_string())
                    .collect::<Vec<_>>(),
            ],
        )?;
        let described: Option<String> = row.get(0);
        let triggers: i64 = row.get(1);
        Ok(described.as_deref() == Some(print) && triggers == tables as i64)
    }

    /// Name the writer's epoch and the deployment's time on a connection, at
    /// each checkout: its commits are fenced by the one and dated by the other.
    pub fn prepare(
        &self,
        client: &mut impl GenericClient,
        epoch: u64,
        now_ns: i64,
    ) -> Result<(), postgres::Error> {
        let epoch = if epoch == 0 {
            String::new()
        } else {
            epoch.to_string()
        };
        client.execute(
            "SELECT set_config($1, $2, false), set_config($3, $4, false)",
            &[
                &self.epoch_setting(),
                &epoch,
                &self.now_setting(),
                &now_ns.to_string(),
            ],
        )?;
        Ok(())
    }

    /// The partition's epoch now, the head row given this deployment as its
    /// origin where it had none: what a writer adopts at start.
    pub fn adopt(&self, client: &mut impl GenericClient) -> Result<u64, postgres::Error> {
        client.execute(
            &format!(
                "UPDATE {} SET origin = $1 WHERE origin = '' AND $1 <> ''",
                self.head
            ),
            &[&origin()],
        )?;
        let epoch: i64 = client
            .query_one(
                &format!("SELECT epoch FROM {} WHERE partition = $1", self.head),
                &[&self.partition],
            )?
            .get(0);
        Ok(epoch as u64)
    }

    /// Take the store over: raise the epoch of its partition and its control
    /// partition, and record who took it over and why (`WriterTookOver`, in
    /// `{prefix}_writer_took_over`), in one transaction; answer the new
    /// epoch, which the writer taking over adopts. Every commit under the
    /// old one fails.
    pub fn take_over(
        &self,
        client: &mut impl GenericClient,
        by: &crate::TookOver,
    ) -> Result<u64, postgres::Error> {
        let mut tx = client.transaction()?;
        crate::end_deposed_writers(&mut tx, &[self.head])?;
        let epoch = self.raise(&mut tx, by)?;
        tx.commit()?;
        Ok(epoch)
    }

    /// Follow the database's timeline for the writer at epoch `mine`
    /// (contract v30, stage C; [`crate::timeline`]): on a new one, the epoch
    /// raised as a takeover raises it, the takeover recorded with
    /// [`crate::timeline::DATABASE_TIMELINE`] by `member`, in one
    /// transaction. Answers what was found, and the identity now.
    pub fn follow_timeline(
        &self,
        client: &mut impl GenericClient,
        mine: u64,
        member: &str,
    ) -> Result<(crate::timeline::Timeline, String), postgres::Error> {
        let mut tx = client.transaction()?;
        let by = crate::TookOver {
            member: member.to_string(),
            why: crate::timeline::DATABASE_TIMELINE.to_string(),
            at_ns: crate::now_ns(),
        };
        let found =
            crate::timeline::follow(&mut tx, self.head, CONTROL, mine, |tx| self.raise(tx, &by))?;
        tx.commit()?;
        Ok(found)
    }

    /// Raise the epoch of the partition and its control partition, and
    /// record who raised it and why, in `tx`: answer the epoch.
    fn raise(
        &self,
        tx: &mut postgres::Transaction<'_>,
        by: &crate::TookOver,
    ) -> Result<u64, postgres::Error> {
        let epoch: i64 = tx
            .query_one(
                &format!(
                    "WITH raised AS (
                        UPDATE {head} SET epoch = (SELECT max(epoch) + 1 FROM {head})
                         WHERE partition IN ($1, '{CONTROL}') RETURNING epoch)
                     SELECT max(epoch) FROM raised",
                    head = self.head
                ),
                &[&self.partition],
            )?
            .get(0);
        let partitions = vec![self.partition.to_string(), CONTROL.to_string()];
        tx.execute(
            &format!(
                "INSERT INTO {}_writer_took_over (epoch, member, partitions, why, origin, at_ns)
                 VALUES ($1, $2, $3, $4, $5, $6)",
                self.prefix
            ),
            &[
                &epoch,
                &by.member,
                &partitions,
                &by.why,
                &origin(),
                &by.at_ns,
            ],
        )?;
        Ok(epoch as u64)
    }

    /// The partition's epoch now, as its head row says: what a writer
    /// compares its own with, to step down once another took it over.
    pub fn head_epoch(&self, client: &mut impl GenericClient) -> Result<u64, postgres::Error> {
        let epoch: i64 = client
            .query_one(
                &format!("SELECT epoch FROM {} WHERE partition = $1", self.head),
                &[&self.partition],
            )?
            .get(0);
        Ok(epoch as u64)
    }

    /// A page of the partition's changes after `after`: each transaction's
    /// rows one change, never split across pages.
    ///
    /// Read by the log's key alone (`enable_seqscan` off for its
    /// transaction): every question asked of a log is a range of one
    /// partition's sequences, and a log small enough that Postgres would
    /// scan it whole once analysed (a few pages) otherwise reads every page
    /// to answer one -- so a page damaged anywhere in it failed every read,
    /// the `control` partition's too, which holds no rows here and is what a
    /// member's probe and followers ask (the tolerant bit-rot class: failover-
    /// repeat 42977259116, the configuration store's log, its first page
    /// flipped, refused the probe's read of `control` with "invalid page in
    /// block 0" until the class gave up). A damaged page still fails the
    /// reads that reach its rows, and the scrub records it.
    pub fn read(
        &self,
        client: &mut impl GenericClient,
        partition: &str,
        after: u64,
        most_bytes: usize,
    ) -> Result<ReadChangesReply, ReadError> {
        if partition != self.partition && partition != CONTROL {
            return Err(ReadError::UnknownPartition(partition.to_string()));
        }
        let unavailable = |failed: postgres::Error| ReadError::Unavailable(failed.to_string());
        let mut tx = client.transaction().map_err(unavailable)?;
        tx.batch_execute("SET LOCAL enable_seqscan = off")
            .map_err(unavailable)?;
        let page = self.read_by_key(&mut tx, partition, after, most_bytes)?;
        tx.commit().map_err(unavailable)?;
        Ok(page)
    }

    fn read_by_key(
        &self,
        client: &mut impl GenericClient,
        partition: &str,
        after: u64,
        most_bytes: usize,
    ) -> Result<ReadChangesReply, ReadError> {
        let unavailable = |failed: postgres::Error| ReadError::Unavailable(failed.to_string());
        let head_row = client
            .query_opt(
                &format!(
                    "SELECT sequence, epoch, origin, coalesce(kept_from, 1) FROM {} WHERE partition = $1",
                    self.head
                ),
                &[&partition],
            )
            .map_err(unavailable)?
            .ok_or_else(|| ReadError::UnknownPartition(partition.to_string()))?;
        let head = head_row.get::<_, i64>(0) as u64;
        let epoch = head_row.get::<_, i64>(1) as u64;
        let own_origin: String = head_row.get(2);
        let kept_from = head_row.get::<_, i64>(3) as u64;

        let lowest: Option<i64> = client
            .query_one(
                &format!(
                    "SELECT min(sequence) FROM {} WHERE partition = $1",
                    self.change_table()
                ),
                &[&partition],
            )
            .map_err(unavailable)?
            .get(0);
        let lowest_kept = lowest.map_or(0, |at| at as u64);
        check_kept(after, lowest_kept.max(kept_from).min(head + 1), head)?;
        const BATCH: i64 = 4096;
        let mut page = Page::new(most_bytes);
        let mut from = after;
        'pages: loop {
            let rows = client
                .query(
                    &format!(
                        "SELECT sequence, epoch, origin, committed_at_ns, kind, body::text,
                                coalesce(digest = sha256(convert_to(body::text, 'UTF8')), true)
                           FROM {} WHERE partition = $1 AND sequence > $2
                          ORDER BY sequence, ordinal LIMIT $3",
                        self.change_table()
                    ),
                    &[&partition, &(from as i64), &BATCH],
                )
                .map_err(unavailable)?;
            if rows.is_empty() {
                break;
            }
            let full = rows.len() as i64 == BATCH;
            let mut groups: Vec<Change> = Vec::new();
            let mut bodies: Vec<Vec<String>> = Vec::new();
            for row in &rows {
                let sequence = row.get::<_, i64>(0) as u64;
                // A row changed beneath the store is never served: the
                // follower stays behind, and the scrub records it.
                if !row.get::<_, bool>(6) {
                    return Err(ReadError::Unavailable(format!(
                        "change {sequence} of partition {partition} does not match its digest: \
                         the record changed beneath the store"
                    )));
                }
                let entry = format!(
                    "{{\"kind\":{},\"row\":{}}}",
                    json_string(row.get::<_, &str>(4)),
                    row.get::<_, &str>(5)
                );
                if groups.last().is_some_and(|last| last.sequence == sequence) {
                    let last = groups.last_mut().expect("a group");
                    last.committed_at_ns = last.committed_at_ns.max(row.get(3));
                    bodies.last_mut().expect("a group").push(entry);
                    continue;
                }
                let origin: Option<String> = row.get(2);
                groups.push(Change {
                    partition: partition.to_string(),
                    sequence,
                    epoch: row.get::<_, i64>(1) as u64,
                    origin: origin.unwrap_or_else(|| own_origin.clone()),
                    committed_at_ns: row.get(3),
                    kind: "rows".to_string(),
                    body: Vec::new(),
                    digest: Vec::new(),
                });
                bodies.push(vec![entry]);
            }
            // A batch that filled may have cut its last transaction short:
            // that one is read again from its start, unless it is the only one.
            if full && groups.len() > 1 {
                groups.pop();
                bodies.pop();
            } else if full {
                let sequence = groups[0].sequence;
                let rest = client
                    .query(
                        &format!(
                            "SELECT kind, body::text FROM {} WHERE partition = $1 AND sequence = $2
                              ORDER BY ordinal OFFSET $3",
                            self.change_table()
                        ),
                        &[&partition, &(sequence as i64), &BATCH],
                    )
                    .map_err(unavailable)?;
                for row in rest {
                    bodies[0].push(format!(
                        "{{\"kind\":{},\"row\":{}}}",
                        json_string(row.get::<_, &str>(0)),
                        row.get::<_, &str>(1)
                    ));
                }
            }
            for (mut change, body) in groups.into_iter().zip(bodies) {
                change.body = format!("[{}]", body.join(",")).into_bytes();
                // The transaction's rows, as carried, verified by whoever
                // applies them.
                change.digest = crate::integrity::digest(&change.body);
                from = change.sequence;
                if !page.take(change) {
                    break 'pages;
                }
            }
            if page.is_full() {
                break;
            }
        }
        Ok(ReadChangesReply {
            changes: page.changes,
            head,
            lowest_kept,
            epoch,
            after_sequence: 0,
        })
    }
}

/// Whether a Postgres error is a commit refused by the fence.
pub fn is_fenced(failed: &postgres::Error) -> bool {
    failed
        .code()
        .is_some_and(|code| code.code() == FENCED_STATE)
}

/// A string as a JSON string literal.
fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The tail of a captured log: each change published on its partition's
/// tail after it commits, at most once, from a connection that listens for
/// the store's commits (`{prefix}_changes`) and reads what is new. It starts
/// at the head: a member that wants what came before reads it.
pub struct Tail;

impl Tail {
    /// Run the tail on a thread of its own, for as long as the process.
    pub fn spawn(
        url: String,
        spec: Spec,
        bus: std::sync::Arc<meridian_bus::Bus>,
        domain: &'static str,
    ) -> std::thread::JoinHandle<()> {
        std::thread::Builder::new()
            .name(format!("{}-tail", spec.prefix))
            .spawn(move || loop {
                if let Err(failed) = Self::run(&url, &spec, &bus, domain) {
                    tracing::warn!(%failed, prefix = spec.prefix, "the change log's tail stopped; starting it again");
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            })
            .expect("a thread for the tail")
    }

    fn run(url: &str, spec: &Spec, bus: &meridian_bus::Bus, domain: &str) -> Result<(), String> {
        use postgres::fallible_iterator::FallibleIterator;
        let config: postgres::Config = url.parse().map_err(|e: postgres::Error| e.to_string())?;
        let (config, tls) = crate::tls::secured(config, crate::tls::names_ssl_mode(url))
            .map_err(|e| e.to_string())?;
        let mut client = config.connect(tls).map_err(|e| e.to_string())?;
        client
            .batch_execute(&format!("LISTEN {}_changes", spec.prefix))
            .map_err(|e| e.to_string())?;
        let mut after: u64 = client
            .query_one(
                &format!("SELECT sequence FROM {} WHERE partition = $1", spec.head),
                &[&spec.partition],
            )
            .map_err(|e| e.to_string())?
            .get::<_, i64>(0) as u64;
        loop {
            // Woken by a commit, or every few seconds whatever: a
            // notification lost is a change published late, never missed.
            {
                let mut heard = client.notifications();
                let _ = heard
                    .timeout_iter(std::time::Duration::from_secs(5))
                    .next()
                    .map_err(|e| e.to_string())?;
                while let Ok(Some(_)) = heard.iter().next() {}
            }
            loop {
                let page = spec
                    .read(&mut client, spec.partition, after, crate::PAGE_MOST)
                    .map_err(|e| e.to_string())?;
                // One transaction is one change here (each transaction's
                // rows share its sequence), so each is its own page.
                for change in &page.changes {
                    crate::publish(bus, domain, after, std::slice::from_ref(change));
                    after = change.sequence;
                }
                if page.changes.is_empty() || after >= page.head {
                    break;
                }
            }
        }
    }
}

/// A partition's state as its rows: each table's rows by primary key, as
/// JSON. What a follower applying the captured log builds, and what the
/// tables hold: the two compared by each store's tests.
pub type State = std::collections::BTreeMap<(String, String), String>;

/// The key of a row of `table`: its primary key's columns, as JSON.
fn key_of(
    keys: &std::collections::BTreeMap<String, Vec<String>>,
    table: &str,
    row: &str,
) -> String {
    let columns = keys.get(table).cloned().unwrap_or_default();
    if columns.is_empty() {
        return row.to_string();
    }
    columns
        .iter()
        .map(|column| json_field(row, column).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("|")
}

/// A top-level field of a JSON object, as its JSON text: enough for the
/// flat rows `to_jsonb` makes.
fn json_field(object: &str, field: &str) -> Option<String> {
    let needle = format!("{}:", json_string(field));
    let at = object.find(&needle)? + needle.len();
    let rest = object[at..].trim_start();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in rest.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' | '[' => depth += 1,
            '}' | ']' if depth == 0 => return Some(rest[..i].trim().to_string()),
            '}' | ']' => depth -= 1,
            ',' if depth == 0 => return Some(rest[..i].trim().to_string()),
            _ => {}
        }
    }
    Some(rest.trim().to_string())
}

impl Spec {
    /// Each captured table's primary key's columns.
    pub fn keys(
        &self,
        client: &mut impl GenericClient,
    ) -> Result<std::collections::BTreeMap<String, Vec<String>>, postgres::Error> {
        let mut keys = std::collections::BTreeMap::new();
        for table in self.tables {
            let columns: Vec<String> = client
                .query(
                    "SELECT a.attname::text FROM pg_index i
                       JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey)
                      WHERE i.indrelid = to_regclass($1) AND i.indisprimary
                      ORDER BY array_position(i.indkey, a.attnum)",
                    &[table],
                )?
                .into_iter()
                .map(|row| row.get(0))
                .collect();
            keys.insert(table.to_string(), columns);
        }
        Ok(keys)
    }

    /// What the captured tables hold now, as a [`State`].
    pub fn state(&self, client: &mut impl GenericClient) -> Result<State, postgres::Error> {
        let keys = self.keys(client)?;
        let mut state = State::new();
        for table in self.tables {
            for row in client.query(&format!("SELECT to_jsonb(t)::text FROM {table} t"), &[])? {
                let row: String = row.get(0);
                // Through jsonb's own text, as the log keeps it.
                state.insert((table.to_string(), key_of(&keys, table, &row)), row);
            }
        }
        Ok(state)
    }

    /// Apply a change of this log to a state: each row's insert, update or
    /// delete, or a table emptied.
    pub fn apply(
        &self,
        keys: &std::collections::BTreeMap<String, Vec<String>>,
        state: &mut State,
        change: &Change,
    ) {
        let body = String::from_utf8_lossy(&change.body);
        for entry in split_array(&body) {
            let kind = json_field(&entry, "kind").unwrap_or_default();
            let kind = kind.trim_matches('"');
            let row = json_field(&entry, "row").unwrap_or_default();
            let (table, op) = kind.split_once('.').unwrap_or((kind, ""));
            match op {
                "insert" | "update" => {
                    // An update that moved its key leaves its old key behind
                    // in this simple follower only if the key changed, which
                    // no captured table's writes do.
                    state.insert((table.to_string(), key_of(keys, table, &row)), row);
                }
                "delete" => {
                    state.remove(&(table.to_string(), key_of(keys, table, &row)));
                }
                "truncate" => state.retain(|(held, _), _| held != table),
                _ => {}
            }
        }
    }
}

/// The elements of a JSON array, each as its text.
fn split_array(array: &str) -> Vec<String> {
    let inner = array.trim().trim_start_matches('[').trim_end_matches(']');
    let mut out = Vec::new();
    let (mut depth, mut in_string, mut escaped, mut start) = (0i32, false, false, 0usize);
    for (i, c) in inner.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' | '[' => depth += 1,
            '}' | ']' => depth -= 1,
            ',' if depth == 0 => {
                out.push(inner[start..i].trim().to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    if !inner[start..].trim().is_empty() {
        out.push(inner[start..].trim().to_string());
    }
    out
}

impl Spec {
    /// The partition's head and the tables' state, read in one snapshot:
    /// what a follower reloads before following from that head.
    pub fn snapshot(&self, client: &mut postgres::Client) -> Result<(u64, State), String> {
        let mut tx = client
            .build_transaction()
            .isolation_level(postgres::IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .map_err(|e| e.to_string())?;
        let head: i64 = tx
            .query_one(
                &format!("SELECT sequence FROM {} WHERE partition = $1", self.head),
                &[&self.partition],
            )
            .map_err(|e| e.to_string())?
            .get(0);
        let state = self.state(&mut tx).map_err(|e| e.to_string())?;
        Ok((head as u64, state))
    }

    /// What every store's tests hold its log to (step 3a's done-when): a
    /// follower that reloaded the partition at `from` and is given the log
    /// after it, in any page split, builds the writer's state. Read in one
    /// snapshot: the tables, the head and every page of the log, the
    /// follower's state built from them and compared with the tables', page
    /// size by page size, with no hole in the sequence. Answers how many
    /// changes it followed.
    pub fn check_follows(
        &self,
        client: &mut postgres::Client,
        from: &(u64, State),
    ) -> Result<u64, String> {
        let mut tx = client
            .build_transaction()
            .isolation_level(postgres::IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .map_err(|e| e.to_string())?;
        let keys = self.keys(&mut tx).map_err(|e| e.to_string())?;
        let held = self.state(&mut tx).map_err(|e| e.to_string())?;
        let mut followed_changes = 0;
        for page in [1usize, 700, 4096, crate::PAGE_MOST] {
            let mut followed = from.1.clone();
            let mut changes = 0u64;
            let mut last = from.0;
            let mut holed = None;
            let reached = crate::catch_up(
                |after| self.read(&mut tx, self.partition, after, page),
                from.0,
                |change| {
                    if change.sequence != last + 1 && holed.is_none() {
                        holed = Some(change.sequence);
                    }
                    last = change.sequence;
                    changes += 1;
                    self.apply(&keys, &mut followed, change);
                },
            )
            .map_err(|e| e.to_string())?;
            if let Some(at) = holed {
                return Err(format!("the log has a hole before {at} (page {page})"));
            }
            if followed != held {
                let missing: Vec<_> = held
                    .keys()
                    .filter(|k| !followed.contains_key(*k))
                    .take(3)
                    .collect();
                let extra: Vec<_> = followed
                    .keys()
                    .filter(|k| !held.contains_key(*k))
                    .take(3)
                    .collect();
                let differ: Vec<_> = held
                    .iter()
                    .filter(|(k, v)| followed.get(*k).is_some_and(|f| f != *v))
                    .map(|(k, _)| k)
                    .take(3)
                    .collect();
                return Err(format!(
                    "page {page}: the follower's state at {reached} is not the tables': \
                     missing {missing:?}, extra {extra:?}, different {differ:?}"
                ));
            }
            followed_changes = changes;
        }
        Ok(followed_changes)
    }

    /// The store's tables, each held to one list or the other: every table
    /// the schema holds whose name the store's tables share a prefix with,
    /// or that `also` names, captured or said why not.
    pub fn check_tables(&self, client: &mut postgres::Client, also: &[&str]) -> Result<(), String> {
        let named: Vec<String> = client
            .query(
                "SELECT table_name::text FROM information_schema.tables
                  WHERE table_schema = current_schema() AND table_type = 'BASE TABLE'",
                &[],
            )
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|row| row.get(0))
            .collect();
        let prefix = format!("{}_", self.prefix);
        for table in named {
            let ours = table.starts_with(&prefix)
                || table == self.prefix
                || also.contains(&table.as_str());
            // The log's own, and integrity at rest's beside it (step 3b):
            // its findings and its scrubs are the log's working records,
            // never the store's changes.
            let own = table == self.head
                || table == format!("{}_change", self.prefix)
                || table == format!("{}_integrity_finding", self.prefix)
                || table == format!("{}_scrub", self.prefix)
                || table.ends_with("_schema_migration");
            if !ours || own {
                continue;
            }
            let listed = self.tables.contains(&table.as_str())
                || self.not_captured.iter().any(|(name, _)| *name == table);
            if !listed {
                return Err(format!(
                    "{table} is neither captured in the change log nor said why not"
                ));
            }
        }
        Ok(())
    }
}

/// A stable fingerprint of the install's SQL (FNV-1a, 64 bits): the same
/// text, the same print, on every build.
fn fingerprint(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("meridian change log {hash:016x}")
}

/// Whether an install lost a race for a lock, and is worth trying again.
fn contended(failed: &postgres::Error) -> bool {
    failed.code().is_some_and(|code| {
        *code == postgres::error::SqlState::T_R_DEADLOCK_DETECTED
            || *code == postgres::error::SqlState::LOCK_NOT_AVAILABLE
    })
}
