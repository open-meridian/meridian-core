//! The replica in Postgres, behind the same trait the in-memory store answers.
//!
//! The trait was written for this. Nothing above it changes, which is the test
//! of whether the seam was in the right place.
//!
//! # Why the driver is synchronous
//!
//! [`Store`](crate::Store) is a synchronous trait and should stay one. The bus
//! runs request handlers on a blocking pool already, precisely so a handler may
//! block on something. An asynchronous store would mean an asynchronous trait,
//! boxed futures at every call site, and a rewrite of two modules to buy what
//! the bus already provides.
//!
//! # The version gate is one statement
//!
//! Reading the held version and then deciding whether to write it is two steps
//! with a gap, and two applies of the same instrument can both pass through the
//! gap. `SELECT ... FOR UPDATE` does not close it either, because a row that
//! does not exist yet locks nothing.
//!
//! So the gate is a conditional upsert: the insert either happens, or conflicts
//! and updates only when the incoming version is higher. Postgres decides, once,
//! under the row lock it takes anyway. `RETURNING` then says which happened,
//! and that is the whole of [`Applied`].

use postgres::types::ToSql;
use postgres::NoTls;
use r2d2_postgres::PostgresConnectionManager;

use crate::store::{Applied, Identifier, Instrument, Result, Store, StoreError};

type Pool = r2d2::Pool<PostgresConnectionManager<NoTls>>;
type Connection = r2d2::PooledConnection<PostgresConnectionManager<NoTls>>;

/// Applied on start. See the file for why there is no migration history.
const SCHEMA: &str = include_str!("../migrations/0001_replica.sql");

/// The replica, in a database.
pub struct PostgresStore {
    pool: Pool,
}

impl PostgresStore {
    /// Open a pool. Does not create the schema; call [`PostgresStore::migrate`].
    pub fn connect(url: &str, pool_size: u32) -> Result<Self> {
        let config: postgres::Config = url.parse().map_err(unavailable)?;
        let manager = PostgresConnectionManager::new(config, NoTls);
        let pool = r2d2::Pool::builder()
            .max_size(pool_size.max(1))
            .build(manager)
            .map_err(unavailable)?;

        Ok(Self { pool })
    }

    /// Create the schema if it is not there.
    ///
    /// Idempotent, and safe to run from every instance on every start: a
    /// replica holds nothing the platform cannot send again, so there is no
    /// history to preserve and nothing to lose to a re-run.
    pub fn migrate(&self) -> Result<()> {
        self.conn()?.batch_execute(SCHEMA).map_err(unavailable)
    }

    fn conn(&self) -> Result<Connection> {
        self.pool.get().map_err(unavailable)
    }

    /// The instruments with these identifiers, in two queries rather than one
    /// per row.
    fn load(conn: &mut Connection, ids: &[String]) -> Result<Vec<Instrument>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let rows = conn
            .query(
                "SELECT instrument_id, asset_class, currency, exchange_mic, description,
                        lifecycle_state, version, valid_from_ns, record_time_ns
                   FROM instrument
                  WHERE instrument_id = ANY($1)
                  ORDER BY instrument_id",
                &[&ids],
            )
            .map_err(unavailable)?;

        let mut instruments: Vec<Instrument> = rows
            .into_iter()
            .map(|row| Instrument {
                instrument_id: row.get(0),
                identifiers: Vec::new(),
                asset_class: row.get(1),
                currency: row.get(2),
                exchange_mic: row.get(3),
                description: row.get(4),
                lifecycle_state: row.get(5),
                version: row.get(6),
                valid_from_ns: row.get(7),
                record_time_ns: row.get(8),
            })
            .collect();

        let identifiers = conn
            .query(
                "SELECT instrument_id, scheme, value, source, valid_from_ns, valid_to_ns
                   FROM instrument_identifier
                  WHERE instrument_id = ANY($1)
                  ORDER BY instrument_id, scheme, value",
                &[&ids],
            )
            .map_err(unavailable)?;

        for row in identifiers {
            let owner: String = row.get(0);
            if let Some(instrument) = instruments
                .iter_mut()
                .find(|instrument| instrument.instrument_id == owner)
            {
                instrument.identifiers.push(Identifier {
                    scheme: row.get(1),
                    value: row.get(2),
                    source: row.get(3),
                    valid_from_ns: row.get(4),
                    valid_to_ns: row.get(5),
                });
            }
        }

        Ok(instruments)
    }
}

impl Store for PostgresStore {
    fn by_id(&self, instrument_id: &str) -> Result<Option<Instrument>> {
        let mut conn = self.conn()?;
        let found = Self::load(&mut conn, &[instrument_id.to_string()])?;
        Ok(found.into_iter().next())
    }

    fn matching(
        &self,
        scheme: &str,
        value: &str,
        source: &str,
        as_of_ns: i64,
    ) -> Result<Vec<Instrument>> {
        let mut conn = self.conn()?;

        let ids: Vec<String> = conn
            .query(
                "SELECT DISTINCT instrument_id
                   FROM instrument_identifier
                  WHERE scheme = $1 AND value = $2 AND source = $3
                    AND valid_from_ns <= $4
                    AND (valid_to_ns IS NULL OR valid_to_ns > $4)",
                &[&scheme, &value, &source, &as_of_ns],
            )
            .map_err(unavailable)?
            .into_iter()
            .map(|row| row.get(0))
            .collect();

        // `load` orders by identifier, which is what makes an ambiguous
        // resolution name the same candidates every time.
        Self::load(&mut conn, &ids)
    }

    fn apply(&self, instrument: Instrument) -> Result<Applied> {
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;

        // Either the insert happens, or it conflicts and updates only for a
        // higher version. One decision, taken by Postgres under the row lock it
        // takes anyway, so two concurrent applies cannot both pass the gate.
        let stored = tx
            .query_opt(
                "INSERT INTO instrument (instrument_id, asset_class, currency, exchange_mic,
                                         description, lifecycle_state, version, valid_from_ns,
                                         record_time_ns)
                      VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                 ON CONFLICT (instrument_id) DO UPDATE
                        SET asset_class = EXCLUDED.asset_class,
                            currency = EXCLUDED.currency,
                            exchange_mic = EXCLUDED.exchange_mic,
                            description = EXCLUDED.description,
                            lifecycle_state = EXCLUDED.lifecycle_state,
                            version = EXCLUDED.version,
                            valid_from_ns = EXCLUDED.valid_from_ns,
                            record_time_ns = EXCLUDED.record_time_ns
                      WHERE instrument.version < EXCLUDED.version
                  RETURNING instrument_id",
                &[
                    &instrument.instrument_id,
                    &instrument.asset_class,
                    &instrument.currency,
                    &instrument.exchange_mic,
                    &instrument.description,
                    &instrument.lifecycle_state,
                    &instrument.version,
                    &instrument.valid_from_ns,
                    &instrument.record_time_ns,
                ],
            )
            .map_err(unavailable)?
            .is_some();

        if !stored {
            tx.commit().map_err(unavailable)?;
            return Ok(Applied::AlreadyCurrent);
        }

        // An amend replaces the set authoritatively, so the old rows go. An
        // identifier absent from this version is absent, which is the whole
        // reason a delta was not the verb.
        tx.execute(
            "DELETE FROM instrument_identifier WHERE instrument_id = $1",
            &[&instrument.instrument_id],
        )
        .map_err(unavailable)?;

        for identifier in &instrument.identifiers {
            let parameters: [&(dyn ToSql + Sync); 6] = [
                &instrument.instrument_id,
                &identifier.scheme,
                &identifier.value,
                &identifier.source,
                &identifier.valid_from_ns,
                &identifier.valid_to_ns,
            ];
            tx.execute(
                "INSERT INTO instrument_identifier
                        (instrument_id, scheme, value, source, valid_from_ns, valid_to_ns)
                 VALUES ($1, $2, $3, $4, $5, $6)",
                &parameters,
            )
            .map_err(unavailable)?;
        }

        tx.commit().map_err(unavailable)?;
        Ok(Applied::Stored)
    }

    fn version_of(&self, instrument_id: &str) -> Result<Option<i64>> {
        let row = self
            .conn()?
            .query_opt(
                "SELECT version FROM instrument WHERE instrument_id = $1",
                &[&instrument_id],
            )
            .map_err(unavailable)?;

        Ok(row.map(|row| row.get(0)))
    }

    fn count(&self) -> Result<usize> {
        let row = self
            .conn()?
            .query_one("SELECT count(*) FROM instrument", &[])
            .map_err(unavailable)?;

        let counted: i64 = row.get(0);
        Ok(counted.max(0) as usize)
    }
}

/// Every database failure is the replica being unavailable.
///
/// Not a loss of meaning: nothing above this trait can act differently on a
/// connection refused than on a syntax error, and a caller given the
/// distinction would only be tempted to.
fn unavailable(failed: impl std::fmt::Display) -> StoreError {
    StoreError::Unavailable(failed.to_string())
}
