//! The ledger in Postgres, behind the same trait the in-memory store answers.
//!
//! Synchronous, for the reason the reference crate's store is: the bus runs
//! request handlers on a blocking pool already, precisely so a handler may
//! block on something.
//!
//! # Where the rules live
//!
//! In two places on purpose, and they say the same thing. The code refuses a
//! holding that names both an instrument and identifiers, or neither, and so
//! does a check constraint. The code is what gives a caller a sentence; the
//! constraint is what makes the rule true of the table no matter which path
//! wrote to it, including a person at a prompt.

use postgres::types::ToSql;
use postgres::{NoTls, Transaction};
use r2d2_postgres::PostgresConnectionManager;

use crate::amounts::{Money, Quantity};
use crate::store::{
    Completion, Counts, Holding, Identifier, Opened, Page, Position, Result, Settled, Statement,
    Store, StoreError,
};

type Pool = r2d2::Pool<PostgresConnectionManager<NoTls>>;
type Connection = r2d2::PooledConnection<PostgresConnectionManager<NoTls>>;

const SCHEMA: &str = include_str!("../migrations/0001_ledger.sql");

/// Columns added after the first release, and therefore absent from a database
/// created before them. `CREATE TABLE IF NOT EXISTS` adds nothing to a table
/// that already exists, so without these a database made last week is missing
/// today's field forever and finds out at query time.
///
/// Each is applied only when it is actually missing, and that guard is the
/// whole point rather than an optimisation. `ALTER TABLE ... ADD COLUMN IF NOT
/// EXISTS` takes an exclusive lock on the table even when the column is already
/// there, so a start that has nothing to do still blocks every live
/// transaction, and deadlocks against one holding a row lock and waiting for
/// this connection. That is the ordinary shape of a deploy: new instances
/// starting while old ones serve. Found by running the tests in parallel, which
/// is the same situation with a shorter fuse.
const ADDITIONS: &[(&str, &str, &str)] = &[
    ("statement", "expected_rows", "integer NOT NULL DEFAULT 0"),
    ("statement", "completed_at_ns", "bigint"),
];

/// Names the schema lock. Distinct from the replica's, so a deployment running
/// both does not have one wait on the other.
const SCHEMA_LOCK: i64 = 0x6b65_726e_656c_0001_u64 as i64;

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

    /// Create the schema if it is not there, under a lock.
    pub fn migrate(&self) -> Result<()> {
        let mut conn = self.conn()?;

        conn.execute("SELECT pg_advisory_lock($1)", &[&SCHEMA_LOCK])
            .map_err(unavailable)?;
        let created = conn
            .batch_execute(SCHEMA)
            .map_err(unavailable)
            .and_then(|()| add_missing_columns(&mut conn));
        let _ = conn.execute("SELECT pg_advisory_unlock($1)", &[&SCHEMA_LOCK]);

        created
    }

    fn conn(&self) -> Result<Connection> {
        self.pool.get().map_err(unavailable)
    }
}

impl Store for PostgresStore {
    fn open(&self, statement: Statement) -> Result<(Statement, Opened, Completion)> {
        let mut conn = self.conn()?;

        // Inserted first and conditionally, so the decision is Postgres' under
        // the unique index rather than ours across a read and a write. Two
        // connectors sending the same statement at once cannot both open it.
        let inserted = conn
            .execute(
                "INSERT INTO statement
                        (statement_id, source, external_statement_id, as_of_date, read_at_ns,
                         expected_rows)
                 VALUES ($1, $2, $3, $4, $5, $6)
                 ON CONFLICT (source, external_statement_id) DO NOTHING",
                &[
                    &statement.statement_id,
                    &statement.source,
                    &statement.external_statement_id,
                    &statement.as_of_date,
                    &statement.read_at_ns,
                    &(statement.expected_rows as i32),
                ],
            )
            .map_err(unavailable)?;

        if inserted == 1 {
            // Nothing is coming, so nothing is outstanding. Marked here, in the
            // same statement that decides it, so a redelivery finds it done.
            let completion = if statement.expected_rows == 0 {
                conn.execute(
                    "UPDATE statement SET completed_at_ns = $1
                      WHERE statement_id = $2 AND completed_at_ns IS NULL",
                    &[&statement.read_at_ns, &statement.statement_id],
                )
                .map_err(unavailable)?;
                Completion::JustCompleted
            } else {
                Completion::Nothing
            };

            return Ok((statement, Opened::Opened, completion));
        }

        let row = conn
            .query_one(
                "SELECT statement_id, source, external_statement_id, as_of_date, read_at_ns,
                        expected_rows
                   FROM statement WHERE source = $1 AND external_statement_id = $2",
                &[&statement.source, &statement.external_statement_id],
            )
            .map_err(unavailable)?;

        Ok((
            Statement {
                statement_id: row.get(0),
                source: row.get(1),
                external_statement_id: row.get(2),
                as_of_date: row.get(3),
                read_at_ns: row.get(4),
                expected_rows: row.get::<_, i32>(5).max(0) as u32,
            },
            Opened::AlreadyRecorded,
            Completion::Nothing,
        ))
    }

    fn statement(&self, statement_id: &str) -> Result<Option<Statement>> {
        let row = self
            .conn()?
            .query_opt(
                "SELECT statement_id, source, external_statement_id, as_of_date, read_at_ns,
                        expected_rows
                   FROM statement WHERE statement_id = $1",
                &[&statement_id],
            )
            .map_err(unavailable)?;

        Ok(row.map(|row| Statement {
            statement_id: row.get(0),
            source: row.get(1),
            external_statement_id: row.get(2),
            as_of_date: row.get(3),
            read_at_ns: row.get(4),
            expected_rows: row.get::<_, i32>(5).max(0) as u32,
        }))
    }

    fn record(&self, holding: Holding, now_ns: i64) -> Result<(Settled, Completion)> {
        holding.validate()?;

        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;

        // Locked for the length of the transaction, so two rows landing at
        // once cannot both see themselves as the one that completed it.
        let statement = tx
            .query_opt(
                "SELECT as_of_date, expected_rows, completed_at_ns
                   FROM statement WHERE statement_id = $1 FOR UPDATE",
                &[&holding.statement_id],
            )
            .map_err(unavailable)?
            .ok_or_else(|| StoreError::UnknownStatement(holding.statement_id.clone()))?;

        let as_of: String = statement.get(0);
        let expected: i32 = statement.get(1);
        let already_completed: Option<i64> = statement.get(2);

        let identifiers = to_json(&holding.unresolved_identifiers);
        tx.execute(
            "INSERT INTO holding (holding_id, statement_id, account_id, instrument_id,
                                  quantity_scaled, value_scaled, currency, escalated, identifiers)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9::text::jsonb)",
            &[
                &holding.holding_id,
                &holding.statement_id,
                &holding.account_id,
                &holding.instrument_id,
                &holding.quantity.scaled(),
                &holding.market_value.scaled(),
                &holding.currency,
                &holding.escalated,
                &identifiers,
            ],
        )
        .map_err(unavailable)?;

        let settled = match holding.instrument_id.clone() {
            None => Settled::Unresolved,
            Some(instrument_id) => settle(&mut tx, &holding, instrument_id, &as_of, now_ns)?,
        };

        let received: i64 = tx
            .query_one(
                "SELECT count(*) FROM holding WHERE statement_id = $1",
                &[&holding.statement_id],
            )
            .map_err(unavailable)?
            .get(0);

        let completion =
            if already_completed.is_none() && expected > 0 && received >= expected as i64 {
                tx.execute(
                    "UPDATE statement SET completed_at_ns = $1 WHERE statement_id = $2",
                    &[&now_ns, &holding.statement_id],
                )
                .map_err(unavailable)?;
                Completion::JustCompleted
            } else {
                Completion::Nothing
            };

        tx.commit().map_err(unavailable)?;
        Ok((settled, completion))
    }

    fn counts(&self, statement_id: &str) -> Result<Counts> {
        let mut conn = self.conn()?;

        if conn
            .query_opt(
                "SELECT 1 FROM statement WHERE statement_id = $1",
                &[&statement_id],
            )
            .map_err(unavailable)?
            .is_none()
        {
            return Err(StoreError::UnknownStatement(statement_id.to_string()));
        }

        let row = conn
            .query_one(
                "SELECT count(*),
                        count(*) FILTER (WHERE instrument_id IS NOT NULL),
                        count(*) FILTER (WHERE instrument_id IS NULL)
                   FROM holding WHERE statement_id = $1",
                &[&statement_id],
            )
            .map_err(unavailable)?;

        let received: i64 = row.get(0);
        let resolved: i64 = row.get(1);
        let unresolved: i64 = row.get(2);

        Ok(Counts {
            received: received as u32,
            resolved: resolved as u32,
            unresolved: unresolved as u32,
        })
    }

    fn page(
        &self,
        account_id: &str,
        include_unresolved: bool,
        limit: usize,
        cursor: &str,
    ) -> Result<Page> {
        let mut conn = self.conn()?;
        let wanted = (limit as i64) + 1;

        let rows = conn
            .query(
                "SELECT account_id, instrument_id, quantity_scaled, value_scaled, currency,
                        last_statement_id, as_of_date, updated_at_ns
                   FROM position
                  WHERE ($1 = '' OR account_id = $1)
                    AND ($2 = '' OR instrument_id > $2)
                  ORDER BY account_id, instrument_id
                  LIMIT $3",
                &[&account_id, &cursor, &wanted],
            )
            .map_err(unavailable)?;

        let mut positions: Vec<Position> = rows
            .into_iter()
            .map(|row| Position {
                account_id: row.get(0),
                instrument_id: row.get(1),
                quantity: Quantity::from_scaled(row.get(2)),
                market_value: Money::from_scaled(row.get(3)),
                currency: row.get(4),
                last_statement_id: row.get(5),
                as_of_date: row.get(6),
                updated_at_ns: row.get(7),
            })
            .collect();

        let more = positions.len() > limit;
        positions.truncate(limit);
        let next_cursor = if more {
            positions
                .last()
                .map(|position| position.instrument_id.clone())
                .unwrap_or_default()
        } else {
            String::new()
        };

        let unresolved = if include_unresolved {
            conn.query(
                "SELECT holding_id, statement_id, account_id, quantity_scaled, value_scaled,
                        currency, escalated, identifiers::text
                   FROM holding
                  WHERE instrument_id IS NULL AND ($1 = '' OR account_id = $1)
                  ORDER BY holding_id",
                &[&account_id],
            )
            .map_err(unavailable)?
            .into_iter()
            .map(|row| Holding {
                holding_id: row.get(0),
                statement_id: row.get(1),
                account_id: row.get(2),
                instrument_id: None,
                unresolved_identifiers: from_json(row.get(7)),
                quantity: Quantity::from_scaled(row.get(3)),
                market_value: Money::from_scaled(row.get(4)),
                currency: row.get(5),
                escalated: row.get(6),
            })
            .collect()
        } else {
            Vec::new()
        };

        Ok(Page {
            positions,
            unresolved,
            next_cursor,
        })
    }

    fn position(&self, account_id: &str, instrument_id: &str) -> Result<Option<Position>> {
        let row = self
            .conn()?
            .query_opt(
                "SELECT account_id, instrument_id, quantity_scaled, value_scaled, currency,
                        last_statement_id, as_of_date, updated_at_ns
                   FROM position WHERE account_id = $1 AND instrument_id = $2",
                &[&account_id, &instrument_id],
            )
            .map_err(unavailable)?;

        Ok(row.map(|row| Position {
            account_id: row.get(0),
            instrument_id: row.get(1),
            quantity: Quantity::from_scaled(row.get(2)),
            market_value: Money::from_scaled(row.get(3)),
            currency: row.get(4),
            last_statement_id: row.get(5),
            as_of_date: row.get(6),
            updated_at_ns: row.get(7),
        }))
    }
}

/// Replace the position, and say what it was.
///
/// Three statements rather than one clever one. The insert makes the row exist
/// and does nothing if it already did; the select then locks a row that is
/// certainly there and reads what it held; the update replaces it. A single
/// upsert would be shorter and could not report the previous value under
/// concurrency, and the previous value is what the event carries.
fn settle(
    tx: &mut Transaction<'_>,
    holding: &Holding,
    instrument_id: String,
    as_of_date: &str,
    now_ns: i64,
) -> Result<Settled> {
    let fresh = tx
        .execute(
            "INSERT INTO position (account_id, instrument_id, quantity_scaled, value_scaled,
                                   currency, last_statement_id, as_of_date, updated_at_ns)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (account_id, instrument_id) DO NOTHING",
            &[
                &holding.account_id,
                &instrument_id,
                &holding.quantity.scaled(),
                &holding.market_value.scaled(),
                &holding.currency,
                &holding.statement_id,
                &as_of_date,
                &now_ns,
            ],
        )
        .map_err(unavailable)?
        == 1;

    let position = Position {
        account_id: holding.account_id.clone(),
        instrument_id: instrument_id.clone(),
        quantity: holding.quantity,
        market_value: holding.market_value,
        currency: holding.currency.clone(),
        last_statement_id: holding.statement_id.clone(),
        as_of_date: as_of_date.to_string(),
        updated_at_ns: now_ns,
    };

    if fresh {
        return Ok(Settled::Changed {
            position,
            previous_quantity: Quantity::ZERO,
        });
    }

    let before = tx
        .query_one(
            "SELECT quantity_scaled, value_scaled, currency
               FROM position WHERE account_id = $1 AND instrument_id = $2
               FOR UPDATE",
            &[&holding.account_id, &instrument_id],
        )
        .map_err(unavailable)?;

    let previous_quantity = Quantity::from_scaled(before.get(0));
    let previous_value = Money::from_scaled(before.get(1));
    let previous_currency: String = before.get(2);

    let parameters: [&(dyn ToSql + Sync); 8] = [
        &holding.quantity.scaled(),
        &holding.market_value.scaled(),
        &holding.currency,
        &holding.statement_id,
        &as_of_date,
        &now_ns,
        &holding.account_id,
        &instrument_id,
    ];
    tx.execute(
        "UPDATE position
            SET quantity_scaled = $1, value_scaled = $2, currency = $3,
                last_statement_id = $4, as_of_date = $5, updated_at_ns = $6
          WHERE account_id = $7 AND instrument_id = $8",
        &parameters,
    )
    .map_err(unavailable)?;

    let moved = previous_quantity != holding.quantity
        || previous_value != holding.market_value
        || previous_currency != holding.currency;

    Ok(if moved {
        Settled::Changed {
            position,
            previous_quantity,
        }
    } else {
        Settled::Unchanged { position }
    })
}

/// The identifiers as JSON, hand-built because there are three string fields
/// and pulling in a serialiser for them would be a dependency to keep current
/// forever.
fn to_json(identifiers: &[Identifier]) -> String {
    let mut out = String::from("[");
    for (index, identifier) in identifiers.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            r#"{{"scheme":{},"value":{},"source":{}}}"#,
            quote(&identifier.scheme),
            quote(&identifier.value),
            quote(&identifier.source)
        ));
    }
    out.push(']');
    out
}

fn quote(value: &str) -> String {
    let mut out = String::from("\"");
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The inverse, equally hand-rolled and equally narrow: it reads exactly what
/// `to_json` writes and nothing else.
fn from_json(text: String) -> Vec<Identifier> {
    let mut identifiers = Vec::new();
    for object in text.trim_matches(['[', ']'].as_slice()).split("},") {
        let mut scheme = String::new();
        let mut value = String::new();
        let mut source = String::new();

        for (key, found) in [
            ("\"scheme\":", &mut scheme),
            ("\"value\":", &mut value),
            ("\"source\":", &mut source),
        ] {
            if let Some(start) = object.find(key) {
                let rest = &object[start + key.len()..];
                if let Some(opening) = rest.find('"') {
                    let body = &rest[opening + 1..];
                    let mut collected = String::new();
                    let mut characters = body.chars();
                    while let Some(character) = characters.next() {
                        match character {
                            '\\' => match characters.next() {
                                Some('n') => collected.push('\n'),
                                Some('r') => collected.push('\r'),
                                Some('t') => collected.push('\t'),
                                Some(escaped) => collected.push(escaped),
                                None => break,
                            },
                            '"' => break,
                            character => collected.push(character),
                        }
                    }
                    *found = collected;
                }
            }
        }

        if !scheme.is_empty() || !value.is_empty() {
            identifiers.push(Identifier {
                scheme,
                value,
                source,
            });
        }
    }
    identifiers
}

/// Add each column in [`ADDITIONS`] that is not already there, and touch the
/// table at all only when one is missing.
fn add_missing_columns(conn: &mut Connection) -> Result<()> {
    for (table, column, definition) in ADDITIONS {
        let present = conn
            .query_opt(
                "SELECT 1 FROM information_schema.columns
                  WHERE table_schema = current_schema()
                    AND table_name = $1 AND column_name = $2",
                &[table, column],
            )
            .map_err(unavailable)?
            .is_some();

        if present {
            continue;
        }

        conn.batch_execute(&format!(
            "ALTER TABLE {table} ADD COLUMN IF NOT EXISTS {column} {definition}"
        ))
        .map_err(unavailable)?;
    }
    Ok(())
}

fn unavailable(failed: impl std::error::Error) -> StoreError {
    // With the cause, because this driver's own message for most failures is
    // the words "db error" and everything useful is one level down.
    let mut detail = failed.to_string();
    let mut cause = failed.source();
    while let Some(next) = cause {
        detail.push_str(": ");
        detail.push_str(&next.to_string());
        cause = next.source();
    }
    StoreError::Unavailable(detail)
}
