//! The street store in Postgres, behind the same trait the in-memory store answers.
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
//!
//! # Numbers
//!
//! A quantity and an amount are `numeric` with no fixed precision, which keeps
//! a value exactly and at the scale it was stated with: 1.50 is stored as 1.50
//! and read back as 1.50, and compares equal to 1.5. They cross the driver as
//! text -- the one form both sides read exactly, and one that needs no
//! dependency for a decimal type -- written from [`Exact`]'s formatting and
//! parsed back by it. Check constraints refuse a value the wire could not have
//! carried, 18 places and 38 digits, whoever writes it.

use postgres::types::ToSql;
use postgres::{GenericClient, IsolationLevel, NoTls, Row, Transaction};
use r2d2_postgres::PostgresConnectionManager;

use crate::amounts::{Exact, Money, Quantity};
use crate::migrations;
use crate::store::{
    from_statement_cursor, statement_cursor, Cause, Chain, Change, Collateral, Completed,
    Completion, Cost, Counts, CustodialPosition, Direction, Encumbrance, Figures, Holding,
    Identifier, Key, Lot, Opened, Page, Read, Result, Scope, Settled, Side, Statement,
    StatementPage, StatementsRead, Store, StoreError, PARTITION,
};

type Pool = r2d2::Pool<PostgresConnectionManager<NoTls>>;
type Connection = r2d2::PooledConnection<PostgresConnectionManager<NoTls>>;

/// Names the schema lock. Distinct from the instrument store's, so a deployment running
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

    /// Apply every migration not yet recorded, under a lock.
    ///
    /// Run once per release by `meridian-runtime migrate`, never by a starting
    /// process: N replicas starting together would race to apply the same
    /// migration, and a process that migrates on start changes a customer's
    /// database because somebody restarted a pod.
    ///
    /// Each migration is recorded at the deployment's time, from `clock`.
    pub fn migrate(&self, clock: &dyn meridian_clock::Clock) -> Result<()> {
        let mut conn = self.conn()?;

        conn.execute("SELECT pg_advisory_lock($1)", &[&SCHEMA_LOCK])
            .map_err(unavailable)?;
        let outcome = apply_migrations(&mut conn, clock);
        let _ = conn.execute("SELECT pg_advisory_unlock($1)", &[&SCHEMA_LOCK]);

        outcome
    }

    /// What a start does instead of migrating: read where the database is and
    /// refuse to serve unless this binary recognises it.
    ///
    /// One read of one table, and no lock, so a start with nothing to do
    /// blocks nothing. That is not an optimisation: the mechanism this
    /// replaces took an exclusive lock on every start, which deadlocks against
    /// live traffic during exactly the rollout it was meant to survive.
    pub fn verify(&self) -> Result<()> {
        let mut conn = self.conn()?;
        let applied = if table_exists(&mut conn, "schema_migration")? {
            applied_version(&mut conn)?
        } else {
            None
        };
        migrations::verify(applied)
    }

    fn conn(&self) -> Result<Connection> {
        self.pool.get().map_err(unavailable)
    }
}

/// A statement's columns, in the order [`statement_of`] reads them.
const STATEMENT_COLUMNS: &str = "statement_id, source, external_statement_id, as_of_date,
        read_at_ns, expected_rows, account_id, external_account_id, institution,
        currency_assumed, completed_at_ns, completion_sequence, completion_previous,
        cause_instance_id, cause_acting_for_subject, cause_correlation_id, cause_causation_id,
        security_interest";

/// A custodial position's columns, in the order [`position_of`] reads them.
const POSITION_COLUMNS: &str = "account_id, instrument_id, side, quantity::text,
        settle_date_quantity::text, market_value::text, currency, last_statement_id,
        as_of_date, updated_at_ns, also_counted_in_cash, cost_basis::text, cost_basis_currency,
        average_cost::text, average_cost_currency, margin_requirement::text,
        margin_requirement_currency, sequence, previous_sequence, removed, last_holding_id";

/// A held position, and the row whose lots it carries.
type Held = (CustodialPosition, Option<String>);

impl Store for PostgresStore {
    fn open(&self, statement: Statement, cause: &Cause) -> Result<(Statement, Opened, Completion)> {
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;

        // Inserted first and conditionally, so the decision is Postgres' under
        // the unique index rather than ours across a read and a write. Two
        // connectors sending the same statement at once cannot both open it.
        let inserted = tx
            .execute(
                "INSERT INTO statement
                        (statement_id, source, external_statement_id, as_of_date, read_at_ns,
                         expected_rows, account_id, external_account_id, institution,
                         currency_assumed, security_interest)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
                 ON CONFLICT (source, external_statement_id) DO NOTHING",
                &[
                    &statement.statement_id,
                    &statement.source,
                    &statement.external_statement_id,
                    &statement.as_of_date,
                    &statement.read_at_ns,
                    &(statement.expected_rows as i32),
                    &statement.account_id,
                    &statement.external_account_id,
                    &statement.institution,
                    &statement.currency_assumed,
                    &statement.security_interest,
                ],
            )
            .map_err(unavailable)?;

        if inserted == 1 {
            insert_figures(&mut tx, &statement)?;
            let mut statement = statement;
            // Nothing is coming, so nothing is outstanding. Marked here, in the
            // same transaction that decides it, so a redelivery finds it done.
            let completion = if statement.expected_rows == 0 {
                let change = next_change(&mut tx, Chain::Statement, &statement.account_id)?;
                complete(&mut tx, &statement.statement_id, change, cause)?;
                statement.completed = Some(Completed {
                    change,
                    cause: cause.clone(),
                });
                Completion::JustCompleted(change)
            } else {
                Completion::Nothing
            };
            tx.commit().map_err(unavailable)?;
            return Ok((statement, Opened::Opened, completion));
        }

        let row = tx
            .query_one(
                &format!(
                    "SELECT {STATEMENT_COLUMNS}
                       FROM statement WHERE source = $1 AND external_statement_id = $2"
                ),
                &[&statement.source, &statement.external_statement_id],
            )
            .map_err(unavailable)?;
        let mut held = statement_of(&row)?;
        held.figures = figures_of(&mut tx, &held.statement_id)?;
        tx.commit().map_err(unavailable)?;

        Ok((held, Opened::AlreadyRecorded, Completion::Nothing))
    }

    fn statement(&self, statement_id: &str) -> Result<Option<Statement>> {
        let mut conn = self.conn()?;
        let row = conn
            .query_opt(
                &format!("SELECT {STATEMENT_COLUMNS} FROM statement WHERE statement_id = $1"),
                &[&statement_id],
            )
            .map_err(unavailable)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let mut statement = statement_of(&row)?;
        statement.figures = figures_of(&mut *conn, statement_id)?;
        Ok(Some(statement))
    }

    fn record(&self, holding: Holding, cause: &Cause) -> Result<(Settled, Completion)> {
        holding.validate()?;

        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;

        // Locked for the length of the transaction, so two rows landing at
        // once cannot both see themselves as the one that completed it.
        let statement = tx
            .query_opt(
                "SELECT as_of_date, expected_rows, completed_at_ns, account_id
                   FROM statement WHERE statement_id = $1 FOR UPDATE",
                &[&holding.statement_id],
            )
            .map_err(unavailable)?
            .ok_or_else(|| StoreError::UnknownStatement(holding.statement_id.clone()))?;

        let as_of: String = statement.get(0);
        let expected: i32 = statement.get(1);
        let already_completed: Option<i64> = statement.get(2);
        let mut account_id: String = statement.get(3);

        // A statement is one account's: one from a plugin before v7 takes its
        // first row's, and a row naming another is refused (W2.2).
        if account_id.is_empty() {
            tx.execute(
                "UPDATE statement SET account_id = $1 WHERE statement_id = $2",
                &[&holding.account_id, &holding.statement_id],
            )
            .map_err(unavailable)?;
            account_id = holding.account_id.clone();
        } else if account_id != holding.account_id {
            return Err(StoreError::AnotherAccount {
                row: holding.account_id.clone(),
                statement: account_id,
            });
        }

        let identifiers = to_json(&holding.unresolved_identifiers);
        let (market_value, currency) = money_columns(&holding.market_value);
        let (cost_basis, cost_basis_currency) = money_columns(&holding.cost.cost_basis);
        let (average_cost, average_cost_currency) = money_columns(&holding.cost.average_cost);
        let (margin_requirement, margin_requirement_currency) =
            money_columns(&holding.cost.margin_requirement);
        let settle_date_quantity = holding.settle_date_quantity.map(|q| q.to_string());
        let available = holding.cost.available_quantity.map(|q| q.to_string());
        let not_available = holding.cost.not_available_quantity.map(|q| q.to_string());
        let parameters: [&(dyn ToSql + Sync); 22] = [
            &holding.holding_id,
            &holding.statement_id,
            &holding.account_id,
            &holding.instrument_id,
            &holding.side.as_str(),
            &holding.quantity.to_string(),
            &settle_date_quantity,
            &market_value,
            &currency,
            &holding.currency_assumed,
            &holding.escalated,
            &identifiers,
            &holding.also_counted_in_cash,
            &cost_basis,
            &cost_basis_currency,
            &average_cost,
            &average_cost_currency,
            &margin_requirement,
            &margin_requirement_currency,
            &available,
            &not_available,
            &holding.cost.available_basis,
        ];
        tx.execute(
            "INSERT INTO holding (holding_id, statement_id, account_id, instrument_id, side,
                                  quantity, settle_date_quantity, market_value, currency,
                                  currency_assumed, escalated, identifiers, also_counted_in_cash,
                                  cost_basis, cost_basis_currency, average_cost,
                                  average_cost_currency, margin_requirement,
                                  margin_requirement_currency, available_quantity,
                                  not_available_quantity, available_basis)
             VALUES ($1, $2, $3, $4, $5, $6::text::numeric, $7::text::numeric,
                     $8::text::numeric, $9, $10, $11, $12::text::jsonb, $13,
                     $14::text::numeric, $15, $16::text::numeric, $17, $18::text::numeric, $19,
                     $20::text::numeric, $21::text::numeric, $22)",
            &parameters,
        )
        .map_err(unavailable)?;
        for (ordinal, lot) in holding.cost.lots.iter().enumerate() {
            let (cost, cost_currency) = money_columns(&lot.cost);
            tx.execute(
                "INSERT INTO holding_lot (holding_id, ordinal, quantity, cost, cost_currency,
                                          acquired_date)
                 VALUES ($1, $2, $3::text::numeric, $4::text::numeric, $5, $6)",
                &[
                    &holding.holding_id,
                    &(ordinal as i32),
                    &lot.quantity.to_string(),
                    &cost,
                    &cost_currency,
                    &lot.acquired_date,
                ],
            )
            .map_err(unavailable)?;
        }
        for (ordinal, held) in holding.cost.encumbrances.iter().enumerate() {
            tx.execute(
                "INSERT INTO holding_encumbrance (holding_id, ordinal, kind, quantity, available,
                                                  source_code, pledgee, held_at, segment, detail)
                 VALUES ($1, $2, $3, $4::text::numeric, $5, $6, $7, $8, $9, $10)",
                &[
                    &holding.holding_id,
                    &(ordinal as i32),
                    &held.kind,
                    &held.quantity.to_string(),
                    &held.available,
                    &held.source_code,
                    &held.pledgee,
                    &held.held_at,
                    &held.segment,
                    &held.detail,
                ],
            )
            .map_err(unavailable)?;
        }

        let settled = match holding.instrument_id.clone() {
            None => Settled::Unresolved,
            Some(instrument_id) => settle(&mut tx, &holding, instrument_id, &as_of, cause)?,
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
                let change = next_change(&mut tx, Chain::Statement, &account_id)?;
                complete(&mut tx, &holding.statement_id, change, cause)?;
                Completion::JustCompleted(change)
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

        counts_of(&mut *conn, statement_id)
    }

    fn page(&self, read: &Read) -> Result<Page> {
        read.scope.admit(&read.account_id)?;
        let after = if read.cursor.is_empty() {
            None
        } else {
            Some(Key::from_cursor(&read.cursor)?)
        };
        let mut conn = self.conn()?;
        // One snapshot, so the page and the number it answers agree.
        let mut tx = conn
            .build_transaction()
            .isolation_level(IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .map_err(unavailable)?;
        let as_of = head(&mut tx)?;
        let wanted = (read.limit as i64) + 1;
        let within = within(&read.scope);
        let since = read.since.map(|since| since as i64);

        // After the cursor's whole key, compared as a row, which is the order
        // the rows come back in: a cursor of the instrument alone skipped every
        // row in a later account whose instrument sorted before it.
        let (after_account, after_instrument, after_side) = match &after {
            None => (String::new(), String::new(), String::new()),
            Some(key) => (
                key.account_id.clone(),
                key.instrument_id.clone(),
                key.side.as_str().to_string(),
            ),
        };
        let rows = tx
            .query(
                &format!(
                    "SELECT {POSITION_COLUMNS}
                       FROM custodial_position
                      WHERE ($1 = '' OR account_id = $1)
                        AND ($7::text[] IS NULL OR account_id = ANY($7))
                        AND (($8::bigint IS NULL AND NOT removed) OR sequence > $8)
                        AND (NOT $2 OR (account_id, instrument_id, side) > ($3, $4, $5))
                      ORDER BY account_id, instrument_id, side
                      LIMIT $6"
                ),
                &[
                    &read.account_id,
                    &after.is_some(),
                    &after_account,
                    &after_instrument,
                    &after_side,
                    &wanted,
                    &within,
                    &since,
                ],
            )
            .map_err(unavailable)?;

        let held: Vec<Held> = rows.iter().map(position_of).collect::<Result<_>>()?;
        let mut positions = with_lots(&mut tx, held)?;

        let more = positions.len() > read.limit;
        positions.truncate(read.limit);
        let next_cursor = match positions.last() {
            Some(last) if more => last.key().cursor(),
            _ => String::new(),
        };

        // With the first page only, so a reader going page by page sees each
        // once.
        let unresolved = if read.include_unresolved && after.is_none() {
            tx.query(
                "SELECT holding_id, statement_id, account_id, side, quantity::text,
                        settle_date_quantity::text, market_value::text, currency,
                        currency_assumed, escalated, identifiers::text, also_counted_in_cash
                   FROM holding
                  WHERE instrument_id IS NULL AND ($1 = '' OR account_id = $1)
                    AND ($2::text[] IS NULL OR account_id = ANY($2))
                  ORDER BY holding_id",
                &[&read.account_id, &within],
            )
            .map_err(unavailable)?
            .iter()
            .map(|row| {
                Ok(Holding {
                    holding_id: row.get(0),
                    statement_id: row.get(1),
                    account_id: row.get(2),
                    instrument_id: None,
                    unresolved_identifiers: from_json(row.get(10)),
                    side: side_of(row, 3)?,
                    quantity: quantity_of(row, 4)?,
                    settle_date_quantity: reported_quantity_of(row, 5)?,
                    market_value: money_of(row, 6, 7)?,
                    currency_assumed: row.get(8),
                    also_counted_in_cash: row.get(11),
                    cost: Cost::default(),
                    escalated: row.get(9),
                })
            })
            .collect::<Result<_>>()?
        } else {
            Vec::new()
        };
        tx.commit().map_err(unavailable)?;

        Ok(Page {
            positions,
            unresolved,
            next_cursor,
            as_of,
        })
    }

    fn statements(&self, read: &StatementsRead) -> Result<StatementPage> {
        read.scope.admit(&read.account_id)?;
        let (after_sequence, after_statement) = if read.cursor.is_empty() {
            (-1_i64, String::new())
        } else {
            let (sequence, statement_id) = from_statement_cursor(&read.cursor)?;
            (sequence as i64, statement_id)
        };
        let mut conn = self.conn()?;
        let mut tx = conn
            .build_transaction()
            .isolation_level(IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .map_err(unavailable)?;
        let as_of = head(&mut tx)?;
        let wanted = (read.limit as i64) + 1;
        let since = read.since.map(|since| since as i64);
        let rows = tx
            .query(
                &format!(
                    "SELECT {STATEMENT_COLUMNS}
                       FROM statement
                      WHERE completed_at_ns IS NOT NULL
                        AND ($1 = '' OR account_id = $1)
                        AND ($2::text[] IS NULL OR account_id = ANY($2))
                        AND ($3 = '' OR as_of_date = $3)
                        AND ($4::bigint IS NULL OR completion_sequence > $4)
                        AND (completion_sequence, statement_id) > ($5, $6)
                      ORDER BY completion_sequence, statement_id
                      LIMIT $7"
                ),
                &[
                    &read.account_id,
                    &within(&read.scope),
                    &read.as_of_date,
                    &since,
                    &after_sequence,
                    &after_statement,
                    &wanted,
                ],
            )
            .map_err(unavailable)?;
        let mut statements: Vec<Statement> =
            rows.iter().map(statement_of).collect::<Result<_>>()?;
        let more = statements.len() > read.limit;
        statements.truncate(read.limit);
        let next_cursor = match statements.last() {
            Some(last) if more => statement_cursor(last),
            _ => String::new(),
        };
        let mut page = Vec::with_capacity(statements.len());
        for mut statement in statements {
            statement.figures = figures_of(&mut tx, &statement.statement_id)?;
            let counts = counts_of(&mut tx, &statement.statement_id)?;
            page.push((statement, counts));
        }
        tx.commit().map_err(unavailable)?;
        Ok(StatementPage {
            statements: page,
            next_cursor,
            as_of,
        })
    }

    fn custodial_position(
        &self,
        account_id: &str,
        instrument_id: &str,
        side: Side,
    ) -> Result<Option<CustodialPosition>> {
        let mut conn = self.conn()?;
        let row = conn
            .query_opt(
                &format!(
                    "SELECT {POSITION_COLUMNS}
                       FROM custodial_position
                      WHERE account_id = $1 AND instrument_id = $2 AND side = $3
                        AND NOT removed"
                ),
                &[&account_id, &instrument_id, &side.as_str()],
            )
            .map_err(unavailable)?;
        let Some(row) = row else {
            return Ok(None);
        };
        Ok(with_lots(&mut *conn, vec![position_of(&row)?])?.pop())
    }

    fn move_positions(
        &self,
        replaced_id: &str,
        instrument_id: &str,
        cause: &Cause,
    ) -> Result<Vec<Settled>> {
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;

        // Locked, so a row recorded against the placeholder while this runs
        // waits for it rather than landing beside a position already moved.
        let rows = tx
            .query(
                &format!(
                    "SELECT {POSITION_COLUMNS}
                       FROM custodial_position WHERE instrument_id = $1 AND NOT removed
                      ORDER BY account_id, side
                        FOR UPDATE"
                ),
                &[&replaced_id],
            )
            .map_err(unavailable)?;
        let held: Vec<Held> = rows.iter().map(position_of).collect::<Result<_>>()?;
        let placeholders = with_lots(&mut tx, held.clone())?
            .into_iter()
            .zip(held.into_iter().map(|(_, holding)| holding))
            .collect::<Vec<_>>();

        let mut settled = Vec::new();
        for (placeholder, holding_id) in placeholders {
            let mut moved = CustodialPosition {
                instrument_id: instrument_id.to_string(),
                updated_at_ns: cause.committed_at_ns,
                ..placeholder.clone()
            };

            // Inserted first and conditionally, as `settle` does, so whether
            // the account already holds the instrument on that side is
            // Postgres' decision under the key rather than ours across a read
            // and a write.
            if insert_position(&mut tx, &moved, holding_id.as_deref(), cause)? {
                moved.last_change = number(&mut tx, &moved, cause)?;
                settled.push(Settled::Changed {
                    position: moved,
                    previous_quantity: Quantity::ZERO,
                });
            } else {
                let row = tx
                    .query_one(
                        &format!(
                            "SELECT {POSITION_COLUMNS}
                               FROM custodial_position
                              WHERE account_id = $1 AND instrument_id = $2 AND side = $3
                                FOR UPDATE"
                        ),
                        &[
                            &moved.account_id,
                            &moved.instrument_id,
                            &moved.side.as_str(),
                        ],
                    )
                    .map_err(unavailable)?;
                let standing = with_lots(&mut tx, vec![position_of(&row)?])?
                    .pop()
                    .expect("one position read");

                if standing.removed || moved.stated_later_than(&standing) {
                    moved.last_change = standing.last_change;
                    update_position(&mut tx, &moved, holding_id.as_deref())?;
                    if standing.removed || moved.differs_from(&standing) {
                        moved.last_change = number(&mut tx, &moved, cause)?;
                        settled.push(Settled::Changed {
                            position: moved,
                            previous_quantity: if standing.removed {
                                Quantity::ZERO
                            } else {
                                standing.quantity
                            },
                        });
                    } else {
                        settled.push(Settled::Unchanged { position: moved });
                    }
                }
            }

            // The placeholder's stays as a tombstone, numbered, so a reader of
            // changes learns it is gone (W2.6, W3.9).
            let mut removed = placeholder.tombstone();
            removed.updated_at_ns = cause.committed_at_ns;
            update_position(&mut tx, &removed, None)?;
            removed.last_change = number(&mut tx, &removed, cause)?;
            settled.push(Settled::Changed {
                position: removed,
                previous_quantity: placeholder.quantity,
            });
        }

        tx.commit().map_err(unavailable)?;
        Ok(settled)
    }

    fn placeholder_instruments(&self) -> Result<Vec<String>> {
        let pattern = format!("{}%", crate::ids::PLACEHOLDER_PREFIX);
        Ok(self
            .conn()?
            .query(
                "SELECT DISTINCT instrument_id FROM custodial_position
                  WHERE instrument_id LIKE $1 AND NOT removed
                  ORDER BY instrument_id",
                &[&pattern],
            )
            .map_err(unavailable)?
            .into_iter()
            .map(|row| row.get(0))
            .collect())
    }
}

/// The partition's number now.
fn head(client: &mut impl GenericClient) -> Result<u64> {
    let row = client
        .query_one(
            "SELECT sequence FROM partition_head WHERE partition = $1",
            &[&PARTITION],
        )
        .map_err(unavailable)?;
    Ok(row.get::<_, i64>(0).max(0) as u64)
}

/// The partition's next number, and the previous change of `chain` for the
/// account, both taken in the change's own transaction: the head's row lock
/// orders every change, and a rollback takes its number back with it, so
/// there are no holes (design decision 5).
fn next_change(tx: &mut Transaction<'_>, chain: Chain, account_id: &str) -> Result<Change> {
    let sequence: i64 = tx
        .query_one(
            "UPDATE partition_head SET sequence = sequence + 1 WHERE partition = $1
             RETURNING sequence",
            &[&PARTITION],
        )
        .map_err(unavailable)?
        .get(0);
    let previous: i64 = tx
        .query_opt(
            "SELECT sequence FROM account_chain WHERE chain = $1 AND account_id = $2 FOR UPDATE",
            &[&chain.as_str(), &account_id],
        )
        .map_err(unavailable)?
        .map(|row| row.get(0))
        .unwrap_or(0);
    tx.execute(
        "INSERT INTO account_chain (chain, account_id, sequence) VALUES ($1, $2, $3)
         ON CONFLICT (chain, account_id) DO UPDATE SET sequence = EXCLUDED.sequence",
        &[&chain.as_str(), &account_id, &sequence],
    )
    .map_err(unavailable)?;
    Ok(Change {
        sequence: sequence as u64,
        previous: previous as u64,
    })
}

/// A position's change numbered, and recorded on it with who made it.
fn number(tx: &mut Transaction<'_>, position: &CustodialPosition, cause: &Cause) -> Result<Change> {
    let change = next_change(tx, Chain::Position, &position.account_id)?;
    tx.execute(
        "UPDATE custodial_position
            SET sequence = $1, previous_sequence = $2, changed_by_instance = $3,
                changed_for_subject = $4
          WHERE account_id = $5 AND instrument_id = $6 AND side = $7",
        &[
            &(change.sequence as i64),
            &(change.previous as i64),
            &cause.instance_id,
            &cause.acting_for_subject,
            &position.account_id,
            &position.instrument_id,
            &position.side.as_str(),
        ],
    )
    .map_err(unavailable)?;
    Ok(change)
}

/// A statement marked complete, with its change and who caused it.
fn complete(
    tx: &mut Transaction<'_>,
    statement_id: &str,
    change: Change,
    cause: &Cause,
) -> Result<()> {
    tx.execute(
        "UPDATE statement
            SET completed_at_ns = $1, completion_sequence = $2, completion_previous = $3,
                cause_instance_id = $4, cause_acting_for_subject = $5,
                cause_correlation_id = $6, cause_causation_id = $7
          WHERE statement_id = $8 AND completed_at_ns IS NULL",
        &[
            &cause.committed_at_ns,
            &(change.sequence as i64),
            &(change.previous as i64),
            &cause.instance_id,
            &cause.acting_for_subject,
            &cause.correlation_id,
            &cause.causation_id,
            &statement_id,
        ],
    )
    .map_err(unavailable)?;
    Ok(())
}

/// A statement's figures and their collateral, as it was opened.
fn insert_figures(tx: &mut Transaction<'_>, statement: &Statement) -> Result<()> {
    for (place, figures) in statement.figures.iter().enumerate() {
        let (buying_power, buying_power_currency) = money_columns(&figures.buying_power);
        let (margin_requirement, margin_requirement_currency) =
            money_columns(&figures.margin_requirement);
        let (maintenance_excess, maintenance_excess_currency) =
            money_columns(&figures.maintenance_excess);
        let (initial_margin, initial_margin_currency) = money_columns(&figures.initial_margin);
        let (variation_margin, variation_margin_currency) =
            money_columns(&figures.variation_margin);
        let (net_liquidation, net_liquidation_currency) = money_columns(&figures.net_liquidation);
        let parameters: [&(dyn ToSql + Sync); 15] = [
            &statement.statement_id,
            &figures.segment,
            &(place as i32),
            &buying_power,
            &buying_power_currency,
            &margin_requirement,
            &margin_requirement_currency,
            &maintenance_excess,
            &maintenance_excess_currency,
            &initial_margin,
            &initial_margin_currency,
            &variation_margin,
            &variation_margin_currency,
            &net_liquidation,
            &net_liquidation_currency,
        ];
        tx.execute(
            "INSERT INTO statement_figures
                    (statement_id, segment, ordinal, buying_power, buying_power_currency,
                     margin_requirement, margin_requirement_currency, maintenance_excess,
                     maintenance_excess_currency, initial_margin, initial_margin_currency,
                     variation_margin, variation_margin_currency, net_liquidation,
                     net_liquidation_currency)
             VALUES ($1, $2, $3, $4::text::numeric, $5, $6::text::numeric, $7,
                     $8::text::numeric, $9, $10::text::numeric, $11, $12::text::numeric, $13,
                     $14::text::numeric, $15)",
            &parameters,
        )
        .map_err(unavailable)?;
        for (ordinal, balance) in figures.collateral.iter().enumerate() {
            let (value, value_currency) = money_columns(&balance.value);
            let (after, after_currency) = money_columns(&balance.value_after_haircut);
            let haircut = balance.haircut.map(|haircut| haircut.to_string());
            let identifiers = to_json(&balance.unresolved_identifiers);
            let parameters: [&(dyn ToSql + Sync); 14] = [
                &statement.statement_id,
                &figures.segment,
                &(ordinal as i32),
                &balance.direction.as_str(),
                &balance.instrument_id,
                &identifiers,
                &balance.quantity.to_string(),
                &value,
                &value_currency,
                &haircut,
                &after,
                &after_currency,
                &balance.held_at,
                &balance.reusable,
            ];
            tx.execute(
                "INSERT INTO statement_collateral
                        (statement_id, segment, ordinal, direction, instrument_id, identifiers,
                         quantity, value, value_currency, haircut, value_after_haircut,
                         value_after_haircut_currency, held_at, reusable)
                 VALUES ($1, $2, $3, $4, $5, $6::text::jsonb, $7::text::numeric,
                         $8::text::numeric, $9, $10::text::numeric, $11::text::numeric, $12, $13,
                         $14)",
                &parameters,
            )
            .map_err(unavailable)?;
        }
    }
    Ok(())
}

/// A statement's figures, by segment, each with its collateral in order.
fn figures_of(client: &mut impl GenericClient, statement_id: &str) -> Result<Vec<Figures>> {
    let mut figures: Vec<Figures> = client
        .query(
            "SELECT segment, buying_power::text, buying_power_currency,
                    margin_requirement::text, margin_requirement_currency,
                    maintenance_excess::text, maintenance_excess_currency,
                    initial_margin::text, initial_margin_currency,
                    variation_margin::text, variation_margin_currency,
                    net_liquidation::text, net_liquidation_currency
               FROM statement_figures WHERE statement_id = $1 ORDER BY ordinal, segment",
            &[&statement_id],
        )
        .map_err(unavailable)?
        .iter()
        .map(|row| {
            Ok(Figures {
                segment: row.get(0),
                buying_power: money_of(row, 1, 2)?,
                margin_requirement: money_of(row, 3, 4)?,
                maintenance_excess: money_of(row, 5, 6)?,
                initial_margin: money_of(row, 7, 8)?,
                variation_margin: money_of(row, 9, 10)?,
                net_liquidation: money_of(row, 11, 12)?,
                collateral: Vec::new(),
            })
        })
        .collect::<Result<_>>()?;
    for row in client
        .query(
            "SELECT segment, direction, instrument_id, identifiers::text, quantity::text,
                    value::text, value_currency, haircut::text, value_after_haircut::text,
                    value_after_haircut_currency, held_at, reusable
               FROM statement_collateral WHERE statement_id = $1 ORDER BY segment, ordinal",
            &[&statement_id],
        )
        .map_err(unavailable)?
    {
        let segment: String = row.get(0);
        let direction: String = row.get(1);
        let balance = Collateral {
            direction: Direction::parse(&direction).ok_or_else(|| {
                StoreError::Unavailable(format!(
                    "a stored direction, {direction}, is neither posted nor received"
                ))
            })?,
            instrument_id: row.get(2),
            unresolved_identifiers: from_json(row.get(3)),
            quantity: quantity_of(&row, 4)?,
            value: money_of(&row, 5, 6)?,
            haircut: reported_quantity_of(&row, 7)?,
            value_after_haircut: money_of(&row, 8, 9)?,
            held_at: row.get(10),
            reusable: row.get(11),
        };
        if let Some(figures) = figures.iter_mut().find(|f| f.segment == segment) {
            figures.collateral.push(balance);
        }
    }
    // In the order the statement gave them.
    Ok(figures)
}

fn counts_of(client: &mut impl GenericClient, statement_id: &str) -> Result<Counts> {
    let row = client
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

/// A plugin's scope as a parameter: the accounts, or NULL for every account.
fn within(scope: &Scope) -> Option<Vec<String>> {
    match scope {
        Scope::Everything => None,
        Scope::Within(accounts) => Some(accounts.iter().cloned().collect()),
    }
}

/// Positions with the lots, sub-balances and available split of the rows that
/// last stated them, read in one go.
fn with_lots(client: &mut impl GenericClient, held: Vec<Held>) -> Result<Vec<CustodialPosition>> {
    let holding_ids: Vec<String> = held.iter().filter_map(|(_, id)| id.clone()).collect();
    let mut lots: std::collections::HashMap<String, Vec<Lot>> = std::collections::HashMap::new();
    let mut encumbrances: std::collections::HashMap<String, Vec<Encumbrance>> =
        std::collections::HashMap::new();
    type Split = (Option<Quantity>, Option<Quantity>, i32);
    let mut splits: std::collections::HashMap<String, Split> = std::collections::HashMap::new();
    if !holding_ids.is_empty() {
        for row in client
            .query(
                "SELECT holding_id, kind, quantity::text, available, source_code, pledgee,
                        held_at, segment, detail
                   FROM holding_encumbrance WHERE holding_id = ANY($1)
                  ORDER BY holding_id, ordinal",
                &[&holding_ids],
            )
            .map_err(unavailable)?
        {
            encumbrances
                .entry(row.get(0))
                .or_default()
                .push(Encumbrance {
                    kind: row.get(1),
                    quantity: quantity_of(&row, 2)?,
                    available: row.get(3),
                    source_code: row.get(4),
                    pledgee: row.get(5),
                    held_at: row.get(6),
                    segment: row.get(7),
                    detail: row.get(8),
                });
        }
        for row in client
            .query(
                "SELECT holding_id, available_quantity::text, not_available_quantity::text,
                        available_basis
                   FROM holding WHERE holding_id = ANY($1)",
                &[&holding_ids],
            )
            .map_err(unavailable)?
        {
            splits.insert(
                row.get(0),
                (
                    reported_quantity_of(&row, 1)?,
                    reported_quantity_of(&row, 2)?,
                    row.get(3),
                ),
            );
        }
        for row in client
            .query(
                "SELECT holding_id, quantity::text, cost::text, cost_currency, acquired_date
                   FROM holding_lot WHERE holding_id = ANY($1) ORDER BY holding_id, ordinal",
                &[&holding_ids],
            )
            .map_err(unavailable)?
        {
            lots.entry(row.get(0)).or_default().push(Lot {
                quantity: quantity_of(&row, 1)?,
                cost: money_of(&row, 2, 3)?,
                acquired_date: row.get(4),
            });
        }
    }
    Ok(held
        .into_iter()
        .map(|(mut position, holding_id)| {
            if let Some(id) = holding_id {
                if let Some(found) = lots.remove(&id) {
                    position.cost.lots = found;
                }
                if let Some(found) = encumbrances.remove(&id) {
                    position.cost.encumbrances = found;
                }
                if let Some((available, not_available, basis)) = splits.remove(&id) {
                    position.cost.available_quantity = available;
                    position.cost.not_available_quantity = not_available;
                    position.cost.available_basis = basis;
                }
            }
            position
        })
        .collect())
}

/// Replace the position, and say what it was.
///
/// Three statements rather than one clever one. The insert makes the row exist
/// and does nothing if it already did; the select then locks a row that is
/// certainly there and reads what it held; the update replaces it. A single
/// upsert would be shorter and could not report the previous value under
/// concurrency, and the previous value is what the event carries. A change
/// is numbered in the same transaction; a row saying what was held takes no
/// number, so the numbers have no holes.
fn settle(
    tx: &mut Transaction<'_>,
    holding: &Holding,
    instrument_id: String,
    as_of_date: &str,
    cause: &Cause,
) -> Result<Settled> {
    let mut position = CustodialPosition {
        account_id: holding.account_id.clone(),
        instrument_id,
        side: holding.side,
        quantity: holding.quantity,
        settle_date_quantity: holding.settle_date_quantity,
        market_value: holding.market_value.clone(),
        also_counted_in_cash: holding.also_counted_in_cash,
        cost: holding.cost.clone(),
        last_statement_id: holding.statement_id.clone(),
        as_of_date: as_of_date.to_string(),
        updated_at_ns: cause.committed_at_ns,
        last_change: Change::default(),
        removed: false,
    };

    if insert_position(tx, &position, Some(&holding.holding_id), cause)? {
        position.last_change = number(tx, &position, cause)?;
        return Ok(Settled::Changed {
            position,
            previous_quantity: Quantity::ZERO,
        });
    }

    let before = tx
        .query_one(
            &format!(
                "SELECT {POSITION_COLUMNS}
                   FROM custodial_position
                  WHERE account_id = $1 AND instrument_id = $2 AND side = $3
                    FOR UPDATE"
            ),
            &[
                &position.account_id,
                &position.instrument_id,
                &position.side.as_str(),
            ],
        )
        .map_err(unavailable)?;
    let before = with_lots(&mut *tx, vec![position_of(&before)?])?
        .pop()
        .expect("one position read");

    position.last_change = before.last_change;
    update_position(tx, &position, Some(&holding.holding_id))?;

    // Compared as numbers, so a statement restating 12.5 as 12.50 announces no
    // change; the row still takes the scale it was restated with.
    Ok(if position.differs_from(&before) {
        position.last_change = number(tx, &position, cause)?;
        Settled::Changed {
            position,
            previous_quantity: if before.removed {
                Quantity::ZERO
            } else {
                before.quantity
            },
        }
    } else {
        Settled::Unchanged { position }
    })
}

/// Make the position exist, unless one is already held under its key. True
/// when this made it.
fn insert_position(
    tx: &mut Transaction<'_>,
    position: &CustodialPosition,
    holding_id: Option<&str>,
    cause: &Cause,
) -> Result<bool> {
    let (market_value, currency) = money_columns(&position.market_value);
    let (cost_basis, cost_basis_currency) = money_columns(&position.cost.cost_basis);
    let (average_cost, average_cost_currency) = money_columns(&position.cost.average_cost);
    let (margin_requirement, margin_requirement_currency) =
        money_columns(&position.cost.margin_requirement);
    let settle_date_quantity = position.settle_date_quantity.map(|q| q.to_string());
    let parameters: [&(dyn ToSql + Sync); 20] = [
        &position.account_id,
        &position.instrument_id,
        &position.side.as_str(),
        &position.quantity.to_string(),
        &settle_date_quantity,
        &market_value,
        &currency,
        &position.last_statement_id,
        &position.as_of_date,
        &position.updated_at_ns,
        &position.also_counted_in_cash,
        &cost_basis,
        &cost_basis_currency,
        &average_cost,
        &average_cost_currency,
        &margin_requirement,
        &margin_requirement_currency,
        &holding_id,
        &cause.instance_id,
        &cause.acting_for_subject,
    ];
    Ok(tx
        .execute(
            "INSERT INTO custodial_position
                    (account_id, instrument_id, side, quantity, settle_date_quantity,
                     market_value, currency, last_statement_id, as_of_date, updated_at_ns,
                     also_counted_in_cash, cost_basis, cost_basis_currency, average_cost,
                     average_cost_currency, margin_requirement, margin_requirement_currency,
                     last_holding_id, changed_by_instance, changed_for_subject)
             VALUES ($1, $2, $3, $4::text::numeric, $5::text::numeric, $6::text::numeric, $7,
                     $8, $9, $10, $11, $12::text::numeric, $13, $14::text::numeric, $15,
                     $16::text::numeric, $17, $18, $19, $20)
             ON CONFLICT (account_id, instrument_id, side) DO NOTHING",
            &parameters,
        )
        .map_err(unavailable)?
        == 1)
}

/// Replace what the position under this key says with what `position` says,
/// its lots being `holding_id`'s.
fn update_position(
    tx: &mut Transaction<'_>,
    position: &CustodialPosition,
    holding_id: Option<&str>,
) -> Result<()> {
    let (market_value, currency) = money_columns(&position.market_value);
    let (cost_basis, cost_basis_currency) = money_columns(&position.cost.cost_basis);
    let (average_cost, average_cost_currency) = money_columns(&position.cost.average_cost);
    let (margin_requirement, margin_requirement_currency) =
        money_columns(&position.cost.margin_requirement);
    let settle_date_quantity = position.settle_date_quantity.map(|q| q.to_string());
    let parameters: [&(dyn ToSql + Sync); 19] = [
        &position.quantity.to_string(),
        &settle_date_quantity,
        &market_value,
        &currency,
        &position.last_statement_id,
        &position.as_of_date,
        &position.updated_at_ns,
        &position.account_id,
        &position.instrument_id,
        &position.side.as_str(),
        &position.also_counted_in_cash,
        &cost_basis,
        &cost_basis_currency,
        &average_cost,
        &average_cost_currency,
        &margin_requirement,
        &margin_requirement_currency,
        &holding_id,
        &position.removed,
    ];
    tx.execute(
        "UPDATE custodial_position
            SET quantity = $1::text::numeric, settle_date_quantity = $2::text::numeric,
                market_value = $3::text::numeric, currency = $4,
                last_statement_id = $5, as_of_date = $6, updated_at_ns = $7,
                also_counted_in_cash = $11, cost_basis = $12::text::numeric,
                cost_basis_currency = $13, average_cost = $14::text::numeric,
                average_cost_currency = $15, margin_requirement = $16::text::numeric,
                margin_requirement_currency = $17, last_holding_id = $18, removed = $19
          WHERE account_id = $8 AND instrument_id = $9 AND side = $10",
        &parameters,
    )
    .map_err(unavailable)?;
    Ok(())
}

/// A statement from a row selected as [`STATEMENT_COLUMNS`] lists it, its
/// figures read apart.
fn statement_of(row: &Row) -> Result<Statement> {
    let completed_at: Option<i64> = row.get(10);
    Ok(Statement {
        statement_id: row.get(0),
        source: row.get(1),
        external_statement_id: row.get(2),
        as_of_date: row.get(3),
        read_at_ns: row.get(4),
        expected_rows: row.get::<_, i32>(5).max(0) as u32,
        account_id: row.get(6),
        external_account_id: row.get(7),
        institution: row.get(8),
        figures: Vec::new(),
        currency_assumed: row.get(9),
        security_interest: row.get(17),
        completed: completed_at.map(|committed_at_ns| Completed {
            change: Change {
                sequence: row.get::<_, i64>(11).max(0) as u64,
                previous: row.get::<_, i64>(12).max(0) as u64,
            },
            cause: Cause {
                instance_id: row.get(13),
                acting_for_subject: row.get(14),
                correlation_id: row.get(15),
                causation_id: row.get(16),
                committed_at_ns,
            },
        }),
    })
}

/// A custodial position from a row selected as [`POSITION_COLUMNS`] lists
/// it, and the row whose lots it carries, read apart.
fn position_of(row: &Row) -> Result<Held> {
    Ok((
        CustodialPosition {
            account_id: row.get(0),
            instrument_id: row.get(1),
            side: side_of(row, 2)?,
            quantity: quantity_of(row, 3)?,
            settle_date_quantity: reported_quantity_of(row, 4)?,
            market_value: money_of(row, 5, 6)?,
            last_statement_id: row.get(7),
            as_of_date: row.get(8),
            updated_at_ns: row.get(9),
            also_counted_in_cash: row.get(10),
            cost: Cost {
                cost_basis: money_of(row, 11, 12)?,
                average_cost: money_of(row, 13, 14)?,
                lots: Vec::new(),
                margin_requirement: money_of(row, 15, 16)?,
                available_quantity: None,
                not_available_quantity: None,
                available_basis: 0,
                encumbrances: Vec::new(),
            },
            last_change: Change {
                sequence: row.get::<_, i64>(17).max(0) as u64,
                previous: row.get::<_, i64>(18).max(0) as u64,
            },
            removed: row.get(19),
        },
        row.get(20),
    ))
}

fn side_of(row: &Row, column: usize) -> Result<Side> {
    let text: String = row.get(column);
    Side::parse(&text).ok_or_else(|| {
        StoreError::Unavailable(format!(
            "a stored side, {text}, is neither long nor short, which the table's check refuses"
        ))
    })
}

fn quantity_of(row: &Row, column: usize) -> Result<Quantity> {
    exact_of(row, column).map(Quantity::new)
}

/// A quantity the venue may not have reported: NULL is absent, not zero.
fn reported_quantity_of(row: &Row, column: usize) -> Result<Option<Quantity>> {
    match row.get::<_, Option<String>>(column) {
        None => Ok(None),
        Some(_) => quantity_of(row, column).map(Some),
    }
}

/// An amount and its currency, or absent where the venue reported none: the
/// two columns are NULL together, which a check holds them to.
fn money_of(row: &Row, amount: usize, currency: usize) -> Result<Option<Money>> {
    match row.get::<_, Option<String>>(amount) {
        None => Ok(None),
        Some(_) => Ok(Some(Money::new(
            exact_of(row, amount)?,
            row.get::<_, Option<String>>(currency).unwrap_or_default(),
        ))),
    }
}

/// An amount as the two columns it is kept in, both NULL where it is absent.
fn money_columns(money: &Option<Money>) -> (Option<String>, Option<String>) {
    match money {
        None => (None, None),
        Some(money) => (Some(money.amount.to_string()), Some(money.currency.clone())),
    }
}

/// A `numeric` read as the text Postgres writes for it. One that does not
/// read is a column holding what no path here writes and the constraints
/// refuse, so it is reported rather than defaulted.
fn exact_of(row: &Row, column: usize) -> Result<Exact> {
    let text: String = row.get(column);
    text.parse().map_err(|why| {
        StoreError::Unavailable(format!(
            "a stored number, {text}, does not read back: {why}"
        ))
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

/// The history table, the baseline for a database that predates it, and then
/// everything outstanding in order.
fn apply_migrations(conn: &mut Connection, clock: &dyn meridian_clock::Clock) -> Result<()> {
    conn.batch_execute(migrations::HISTORY)
        .map_err(unavailable)?;
    adopt_existing_schema(conn, clock)?;

    let applied = applied_version(conn)?;
    for migration in migrations::MIGRATIONS {
        if applied.is_some_and(|at| at >= migration.version) {
            continue;
        }

        // The migration and the row recording it commit together. A failure
        // part way leaves everything before it applied and recorded, and the
        // one that failed neither, so running again resumes rather than
        // repeats.
        let mut tx = conn.transaction().map_err(unavailable)?;
        tx.batch_execute(migration.sql).map_err(unavailable)?;
        migrations::record(&mut tx, migration, clock.now_ns())?;
        tx.commit().map_err(unavailable)?;
    }
    Ok(())
}

/// Tables that mean the street store's schema is already here, whatever record of it
/// exists. `position` is in the list because a database old enough to carry
/// that name is exactly the one adoption is for.
const LEDGER_TABLES: &[&str] = &["statement", "holding", "custodial_position", "position"];

/// A database made before this history existed has the tables and no record of
/// them. Recording the baseline is right where re-running it would be wrong:
/// the first migration creates tables that already hold statements, and
/// creating `custodial_position` fresh beside an old `position` leaves a full
/// table and an empty one with nothing to say which is which.
///
/// Recognised by the tables themselves rather than by a flag, because a flag
/// would have to have been written by the code that did not have one.
fn adopt_existing_schema(conn: &mut Connection, clock: &dyn meridian_clock::Clock) -> Result<()> {
    if applied_version(conn)?.is_some() {
        return Ok(());
    }

    let mut present = false;
    for table in LEDGER_TABLES {
        if table_exists(conn, table)? {
            present = true;
            break;
        }
    }
    if !present {
        return Ok(());
    }

    let baseline = &migrations::MIGRATIONS[0];
    let mut tx = conn.transaction().map_err(unavailable)?;
    migrations::record(&mut tx, baseline, clock.now_ns())?;
    tx.commit().map_err(unavailable)?;
    Ok(())
}

fn applied_version(conn: &mut Connection) -> Result<Option<i64>> {
    let row = conn
        .query_opt("SELECT max(version) FROM schema_migration", &[])
        .map_err(unavailable)?;
    Ok(row.and_then(|row| row.get::<_, Option<i64>>(0)))
}

fn table_exists(conn: &mut Connection, table: &str) -> Result<bool> {
    Ok(conn
        .query_opt(
            "SELECT 1 FROM information_schema.tables
              WHERE table_schema = current_schema() AND table_name = $1",
            &[&table],
        )
        .map_err(unavailable)?
        .is_some())
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
