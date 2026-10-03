//! The instrument store in Postgres, behind the same trait the in-memory store answers.
//!
//! # Why the driver is synchronous
//!
//! [`Store`](crate::Store) is a synchronous trait and should stay one. The bus
//! runs request handlers on a blocking pool already, precisely so a handler may
//! block on something.
//!
//! # The version gate is one statement
//!
//! A record's next version is written by an `UPDATE ... WHERE version = $expected`,
//! so two writers against one version cannot both pass: Postgres decides under
//! the row lock it takes anyway, and the loser's row count is zero. Its
//! identifiers, sources and offers are written whole in the same transaction,
//! with the version's entry in the history.
//!
//! # One record per minted set
//!
//! The unique index on `mint_key` decides, as the one on a placeholder's set
//! did: two resolves of one set racing both reach the insert, and the second
//! reads back what the first stored.

use meridian_domain::asset_class;
use meridian_domain::v1::AssetClass;
use postgres::types::ToSql;
use postgres::NoTls;
use r2d2_postgres::PostgresConnectionManager;

use crate::store::{
    Asked, Change, Conflict, Field, Identifier, Instrument, Offer, Replaced, Result, Source, Stood,
    Store, StoreError, Version, Written,
};

type Pool = r2d2::Pool<PostgresConnectionManager<NoTls>>;
type Connection = r2d2::PooledConnection<PostgresConnectionManager<NoTls>>;

/// Applied by `migrate`, in order.
const SCHEMA: &[&str] = &[
    include_str!("../migrations/0001_instrument.sql"),
    include_str!("../migrations/0002_placeholder.sql"),
    include_str!("../migrations/0003_records.sql"),
];

/// A table the newest file makes. A database that has it has them all, so a
/// start can check for this one and refuse a database an older release
/// migrated, rather than failing later on the first record.
const NEWEST_TABLE: &str = "instrument_conflict";

/// Names the schema lock. An arbitrary constant, and it only has to be the same
/// one in every process that creates this schema.
const SCHEMA_LOCK: i64 = 0x6d65_7269_6469_616e_u64 as i64;

/// What a value applied from the platform before v10 is sourced as (Q8).
fn from_the_platform(version: i64) -> String {
    format!("the platform, record version {version}")
}

/// What an identifier a placeholder carried before v10 is sourced as (Q8).
const REPORTED_BEFORE_V10: &str = "reported before contract v10";

/// The instrument store, in a database.
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

    /// What a start does instead of migrating: check the schema is there.
    ///
    /// A deployment gives the runtime no right to create a table, so a start
    /// that tried would fail with `permission denied for schema public` and
    /// nothing saying which command fixes it. One read, no lock, and a
    /// sentence naming the fix.
    pub fn verify(&self) -> Result<()> {
        let mut conn = self.conn()?;
        let present = conn
            .query_opt(
                "SELECT 1 FROM information_schema.tables
                  WHERE table_schema = current_schema() AND table_name = $1",
                &[&NEWEST_TABLE],
            )
            .map_err(unavailable)?
            .is_some();

        if present {
            return Ok(());
        }
        Err(StoreError::Unavailable(
            "the instrument store's database has no schema, or only what an older release \
             made. Run `meridian-instrument migrate` before starting."
                .into(),
        ))
    }

    /// Create the schema if it is not there, and source what an older release
    /// held (Q8).
    ///
    /// Idempotent, and safe to run from every instance on every start: every
    /// file is additions under `IF NOT EXISTS`, and what is sourced is sourced
    /// once, for a record with no history yet. Under an advisory lock, because
    /// `IF NOT EXISTS` is not the concurrency answer it reads as.
    pub fn migrate(&self) -> Result<()> {
        let mut conn = self.conn()?;

        conn.execute("SELECT pg_advisory_lock($1)", &[&SCHEMA_LOCK])
            .map_err(unavailable)?;

        let created = SCHEMA
            .iter()
            .try_for_each(|file| conn.batch_execute(file).map_err(unavailable))
            .and_then(|()| Self::classes_to_the_enum(&mut conn))
            .and_then(|()| Self::sources_from_before_v10(&mut conn));

        // Released whether or not the schema went in, so a failure does not
        // leave every other instance waiting on a lock nobody holds usefully.
        let _ = conn.execute("SELECT pg_advisory_unlock($1)", &[&SCHEMA_LOCK]);

        created
    }

    /// The asset class was free text until the enum
    /// (sdk-contract/asset-class-is-an-enum). Rewrites each class the columns
    /// hold that is not already the enum's name: to the class it plainly
    /// meant, or to none when it meant nothing plainly, reported rather than
    /// guessed. A re-run finds nothing.
    fn classes_to_the_enum(conn: &mut Connection) -> Result<()> {
        for table in ["instrument", "instrument_placeholder"] {
            let held = conn
                .query(
                    &format!(
                        "SELECT DISTINCT asset_class FROM {table} \
                          WHERE asset_class <> '' AND asset_class NOT LIKE 'ASSET\\_CLASS\\_%'"
                    ),
                    &[],
                )
                .map_err(unavailable)?;
            for row in held {
                let was: String = row.get(0);
                let class = asset_class::legacy(&was);
                let now = asset_class::name(class.unwrap_or(AssetClass::Unspecified));
                let changed = conn
                    .execute(
                        &format!("UPDATE {table} SET asset_class = $1 WHERE asset_class = $2"),
                        &[&now, &was],
                    )
                    .map_err(unavailable)?;
                match class {
                    Some(_) => {
                        tracing::info!(table, was, now, changed, "asset class mapped to the enum")
                    }
                    None => tracing::warn!(
                        table,
                        was,
                        changed,
                        "asset class is not one the enum defines and was cleared; \
                         complete it at the dashboard's Instruments page"
                    ),
                }
            }
        }
        Ok(())
    }

    /// Q8 of spec/a-deployment-completes-its-instrument-records, once per
    /// record with no history: a record applied from the platform keeps its
    /// values, each sourced "the platform, record version N" with no person,
    /// in force; a placeholder became a record with no values (0003), its
    /// identifiers sourced as reported before v10 and the asset class it
    /// kept, where it kept one, an offer. Each starts its history with a
    /// `migrate` version saying what it held.
    fn sources_from_before_v10(conn: &mut Connection) -> Result<()> {
        let ids: Vec<String> = conn
            .query(
                "SELECT instrument_id FROM instrument record
                  WHERE NOT EXISTS (SELECT 1 FROM instrument_version version
                                     WHERE version.instrument_id = record.instrument_id)
                  ORDER BY instrument_id",
                &[],
            )
            .map_err(unavailable)?
            .into_iter()
            .map(|row| row.get(0))
            .collect();
        if ids.is_empty() {
            return Ok(());
        }
        let placeholder_classes: std::collections::HashMap<String, String> = conn
            .query(
                "SELECT placeholder_id, asset_class FROM instrument_placeholder
                  WHERE asset_class <> '' AND placeholder_id = ANY($1)",
                &[&ids],
            )
            .map_err(unavailable)?
            .into_iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect();
        let minted: std::collections::HashSet<String> = conn
            .query(
                "SELECT instrument_id FROM instrument
                  WHERE mint_key IS NOT NULL AND instrument_id = ANY($1)",
                &[&ids],
            )
            .map_err(unavailable)?
            .into_iter()
            .map(|row| row.get(0))
            .collect();

        let mut sourced = 0usize;
        for mut record in Self::load(conn, &ids)? {
            let was_placeholder = minted.contains(&record.instrument_id);
            let words = if was_placeholder {
                REPORTED_BEFORE_V10.to_string()
            } else {
                from_the_platform(record.version)
            };
            let mut changes = Vec::new();
            for field in [Field::AssetClass, Field::Currency, Field::Description] {
                let value = record.value(field).to_string();
                if value.is_empty() {
                    continue;
                }
                record.set_source(Source {
                    field,
                    identifier: None,
                    source: words.clone(),
                    person: String::new(),
                    instance_id: String::new(),
                    recorded_at_ns: record.record_time_ns,
                    note: String::new(),
                });
                changes.push(Change {
                    field: field.name().into(),
                    scheme: String::new(),
                    namespace: String::new(),
                    before: String::new(),
                    after: if field == Field::AssetClass {
                        crate::record::class_words(&value)
                    } else {
                        value
                    },
                    source: words.clone(),
                });
            }
            for identifier in record.identifiers.clone() {
                let asked = identifier.asked();
                record.set_source(Source {
                    field: Field::Identifier,
                    identifier: Some(asked.clone()),
                    source: words.clone(),
                    person: String::new(),
                    instance_id: String::new(),
                    recorded_at_ns: record.record_time_ns,
                    note: String::new(),
                });
                changes.push(crate::resolve::identifier_change(&asked, &words));
            }
            if let Some(class) = placeholder_classes.get(&record.instrument_id) {
                record.offers.push(Offer {
                    field: Field::AssetClass,
                    value: class.clone(),
                    identifier: None,
                    source: REPORTED_BEFORE_V10.into(),
                    instance_id: String::new(),
                    offered_at_ns: record.record_time_ns,
                });
            }
            let entry = Version {
                instrument_id: record.instrument_id.clone(),
                version: record.version,
                operation: "migrate".into(),
                changes,
                person: String::new(),
                instance_id: String::new(),
                note: String::new(),
                merged_instrument_id: String::new(),
                record_time_ns: record.record_time_ns,
            };
            let mut tx = conn.transaction().map_err(unavailable)?;
            Self::write_parts(&mut tx, &record)?;
            Self::write_version(&mut tx, &entry)?;
            tx.commit().map_err(unavailable)?;
            sourced += 1;
        }
        tracing::info!(
            sourced,
            "records held before contract v10 sourced; the dashboard's Instruments page lists \
             what the book cannot use"
        );
        Ok(())
    }

    fn conn(&self) -> Result<Connection> {
        self.pool.get().map_err(unavailable)
    }

    /// The records with these IDs, whole, ordered by ID.
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
                sources: Vec::new(),
                offers: Vec::new(),
            })
            .collect();
        let index: std::collections::HashMap<String, usize> = instruments
            .iter()
            .enumerate()
            .map(|(at, record)| (record.instrument_id.clone(), at))
            .collect();

        for row in conn
            .query(
                "SELECT instrument_id, scheme, value, source, valid_from_ns, valid_to_ns
                   FROM instrument_identifier
                  WHERE instrument_id = ANY($1)
                  ORDER BY instrument_id, scheme, source, value",
                &[&ids],
            )
            .map_err(unavailable)?
        {
            let owner: String = row.get(0);
            if let Some(&at) = index.get(&owner) {
                instruments[at].identifiers.push(Identifier {
                    scheme: row.get(1),
                    value: row.get(2),
                    source: row.get(3),
                    valid_from_ns: row.get(4),
                    valid_to_ns: row.get(5),
                });
            }
        }

        for row in conn
            .query(
                "SELECT instrument_id, field, scheme, namespace, value, source, person,
                        instance_id, recorded_at_ns, note
                   FROM instrument_value_source
                  WHERE instrument_id = ANY($1)
                  ORDER BY instrument_id, field, scheme, namespace, value",
                &[&ids],
            )
            .map_err(unavailable)?
        {
            let owner: String = row.get(0);
            let Some(field) = Field::parse(row.get::<_, &str>(1)) else {
                continue;
            };
            if let Some(&at) = index.get(&owner) {
                instruments[at].sources.push(Source {
                    field,
                    identifier: identifier_of(field, row.get(2), row.get(3), row.get(4)),
                    source: row.get(5),
                    person: row.get(6),
                    instance_id: row.get(7),
                    recorded_at_ns: row.get(8),
                    note: row.get(9),
                });
            }
        }

        for row in conn
            .query(
                "SELECT instrument_id, field, scheme, namespace, value, source, instance_id,
                        offered_at_ns
                   FROM instrument_offer
                  WHERE instrument_id = ANY($1)
                  ORDER BY instrument_id, offered_at_ns, field, value",
                &[&ids],
            )
            .map_err(unavailable)?
        {
            let owner: String = row.get(0);
            let Some(field) = Field::parse(row.get::<_, &str>(1)) else {
                continue;
            };
            if let Some(&at) = index.get(&owner) {
                let value: String = row.get(4);
                instruments[at].offers.push(Offer {
                    field,
                    identifier: identifier_of(field, row.get(2), row.get(3), &value),
                    value,
                    source: row.get(5),
                    instance_id: row.get(6),
                    offered_at_ns: row.get(7),
                });
            }
        }

        Ok(instruments)
    }

    /// A record's identifiers, sources and offers, written whole in place of
    /// what was there.
    fn write_parts(tx: &mut postgres::Transaction<'_>, record: &Instrument) -> Result<()> {
        let id = &record.instrument_id;
        for table in [
            "instrument_identifier",
            "instrument_value_source",
            "instrument_offer",
        ] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE instrument_id = $1"),
                &[id],
            )
            .map_err(unavailable)?;
        }
        for identifier in &record.identifiers {
            let parameters: [&(dyn ToSql + Sync); 6] = [
                id,
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
        for source in &record.sources {
            let (scheme, namespace, value) = parts_of(source.identifier.as_ref());
            tx.execute(
                "INSERT INTO instrument_value_source
                        (instrument_id, field, scheme, namespace, value, source, person,
                         instance_id, recorded_at_ns, note)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                 ON CONFLICT DO NOTHING",
                &[
                    id,
                    &source.field.name(),
                    &scheme,
                    &namespace,
                    &value,
                    &source.source,
                    &source.person,
                    &source.instance_id,
                    &source.recorded_at_ns,
                    &source.note,
                ],
            )
            .map_err(unavailable)?;
        }
        for offer in &record.offers {
            let (scheme, namespace, _) = parts_of(offer.identifier.as_ref());
            tx.execute(
                "INSERT INTO instrument_offer
                        (instrument_id, field, scheme, namespace, value, source, instance_id,
                         offered_at_ns)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                 ON CONFLICT DO NOTHING",
                &[
                    id,
                    &offer.field.name(),
                    &scheme,
                    &namespace,
                    &offer.value,
                    &offer.source,
                    &offer.instance_id,
                    &offer.offered_at_ns,
                ],
            )
            .map_err(unavailable)?;
        }
        Ok(())
    }

    fn write_version(tx: &mut postgres::Transaction<'_>, entry: &Version) -> Result<()> {
        let changes = serde_json::to_string(&entry.changes)
            .map_err(|failed| StoreError::Unavailable(failed.to_string()))?;
        tx.execute(
            "INSERT INTO instrument_version
                    (instrument_id, version, operation, changes, person, instance_id, note,
                     merged_instrument_id, record_time_ns)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT DO NOTHING",
            &[
                &entry.instrument_id,
                &entry.version,
                &entry.operation,
                &changes,
                &entry.person,
                &entry.instance_id,
                &entry.note,
                &entry.merged_instrument_id,
                &entry.record_time_ns,
            ],
        )
        .map_err(unavailable)?;
        Ok(())
    }

    fn by_mint_key(conn: &mut Connection, key: &str) -> Result<Option<Instrument>> {
        let id: Option<String> = conn
            .query_opt(
                "SELECT instrument_id FROM instrument WHERE mint_key = $1",
                &[&key],
            )
            .map_err(unavailable)?
            .map(|row| row.get(0));
        match id {
            Some(id) => Ok(Self::load(conn, &[id])?.into_iter().next()),
            None => Ok(None),
        }
    }
}

fn identifier_of(field: Field, scheme: String, namespace: String, value: &str) -> Option<Asked> {
    (field == Field::Identifier).then(|| Asked {
        scheme,
        value: value.to_string(),
        source: namespace,
    })
}

fn parts_of(identifier: Option<&Asked>) -> (String, String, String) {
    identifier
        .map(|asked| {
            (
                asked.scheme.clone(),
                asked.source.clone(),
                asked.value.clone(),
            )
        })
        .unwrap_or_default()
}

impl Store for PostgresStore {
    fn by_id(&self, instrument_id: &str) -> Result<Option<Instrument>> {
        let mut conn = self.conn()?;
        Ok(Self::load(&mut conn, &[instrument_id.to_string()])?
            .into_iter()
            .next())
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
        // `load` orders by ID, which is what makes an ambiguous resolution
        // name the same candidates every time.
        Self::load(&mut conn, &ids)
    }

    fn all(&self) -> Result<Vec<Instrument>> {
        let mut conn = self.conn()?;
        let ids: Vec<String> = conn
            .query(
                "SELECT instrument_id FROM instrument ORDER BY instrument_id",
                &[],
            )
            .map_err(unavailable)?
            .into_iter()
            .map(|row| row.get(0))
            .collect();
        Self::load(&mut conn, &ids)
    }

    fn count(&self) -> Result<usize> {
        let row = self
            .conn()?
            .query_one("SELECT count(*) FROM instrument", &[])
            .map_err(unavailable)?;
        let counted: i64 = row.get(0);
        Ok(counted.max(0) as usize)
    }

    fn mint(
        &self,
        candidate: Instrument,
        set_key: &str,
        first: Version,
    ) -> Result<(Instrument, Stood)> {
        let mut conn = self.conn()?;
        if let Some(held) = Self::by_mint_key(&mut conn, set_key)? {
            return Ok((held, Stood::AlreadyHeld));
        }

        let mut tx = conn.transaction().map_err(unavailable)?;
        let inserted = tx
            .execute(
                "INSERT INTO instrument (instrument_id, asset_class, currency, exchange_mic,
                                         description, lifecycle_state, version, valid_from_ns,
                                         record_time_ns, mint_key)
                      VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                 ON CONFLICT (mint_key) WHERE mint_key IS NOT NULL DO NOTHING",
                &[
                    &candidate.instrument_id,
                    &candidate.asset_class,
                    &candidate.currency,
                    &candidate.exchange_mic,
                    &candidate.description,
                    &candidate.lifecycle_state,
                    &candidate.version,
                    &candidate.valid_from_ns,
                    &candidate.record_time_ns,
                    &set_key,
                ],
            )
            .map_err(unavailable)?
            == 1;
        if inserted {
            // In the same transaction, so nobody reads a record without the
            // identifiers it was minted for.
            Self::write_parts(&mut tx, &candidate)?;
            Self::write_version(&mut tx, &first)?;
        }
        tx.commit().map_err(unavailable)?;
        if inserted {
            return Ok((candidate, Stood::Minted));
        }

        let held = Self::by_mint_key(&mut conn, set_key)?.ok_or_else(|| {
            StoreError::Unavailable(
                "a record conflicted on its identifier set and then could not be read".into(),
            )
        })?;
        Ok((held, Stood::AlreadyHeld))
    }

    fn write(&self, record: Instrument, expected: i64, entry: Version) -> Result<Written> {
        let mut conn = self.conn()?;
        let mut tx = conn.transaction().map_err(unavailable)?;
        let updated = tx
            .execute(
                "UPDATE instrument
                    SET asset_class = $2, currency = $3, exchange_mic = $4, description = $5,
                        lifecycle_state = $6, version = $7, valid_from_ns = $8,
                        record_time_ns = $9
                  WHERE instrument_id = $1 AND version = $10",
                &[
                    &record.instrument_id,
                    &record.asset_class,
                    &record.currency,
                    &record.exchange_mic,
                    &record.description,
                    &record.lifecycle_state,
                    &record.version,
                    &record.valid_from_ns,
                    &record.record_time_ns,
                    &expected,
                ],
            )
            .map_err(unavailable)?;
        if updated == 0 {
            let held: Option<i64> = tx
                .query_opt(
                    "SELECT version FROM instrument WHERE instrument_id = $1",
                    &[&record.instrument_id],
                )
                .map_err(unavailable)?
                .map(|row| row.get(0));
            tx.rollback().map_err(unavailable)?;
            return Ok(match held {
                Some(held) => Written::Stale { held },
                None => Written::Missing,
            });
        }
        Self::write_parts(&mut tx, &record)?;
        Self::write_version(&mut tx, &entry)?;
        tx.commit().map_err(unavailable)?;
        Ok(Written::Stored)
    }

    fn history(&self, instrument_id: &str) -> Result<Vec<Version>> {
        let rows = self
            .conn()?
            .query(
                "SELECT version, operation, changes, person, instance_id, note,
                        merged_instrument_id, record_time_ns
                   FROM instrument_version
                  WHERE instrument_id = $1
                  ORDER BY version DESC",
                &[&instrument_id],
            )
            .map_err(unavailable)?;
        Ok(rows
            .into_iter()
            .map(|row| Version {
                instrument_id: instrument_id.to_string(),
                version: row.get(0),
                operation: row.get(1),
                changes: serde_json::from_str(row.get::<_, &str>(2)).unwrap_or_default(),
                person: row.get(3),
                instance_id: row.get(4),
                note: row.get(5),
                merged_instrument_id: row.get(6),
                record_time_ns: row.get(7),
            })
            .collect())
    }

    fn note_conflict(&self, conflict: Conflict) -> Result<()> {
        let identifiers = serde_json::to_string(
            &conflict
                .identifiers
                .iter()
                .map(|asked| {
                    [
                        asked.scheme.as_str(),
                        asked.source.as_str(),
                        asked.value.as_str(),
                    ]
                })
                .collect::<Vec<_>>(),
        )
        .map_err(|failed| StoreError::Unavailable(failed.to_string()))?;
        let ids = serde_json::to_string(&conflict.instrument_ids)
            .map_err(|failed| StoreError::Unavailable(failed.to_string()))?;
        self.conn()?
            .execute(
                "INSERT INTO instrument_conflict
                        (conflict_key, identifiers, instrument_ids, reported_by, first_seen_ns,
                         last_seen_ns)
                 VALUES ($1, $2, $3, $4, $5, $6)
                 ON CONFLICT (conflict_key) DO UPDATE
                        SET instrument_ids = EXCLUDED.instrument_ids,
                            last_seen_ns = EXCLUDED.last_seen_ns,
                            reported_by = CASE WHEN instrument_conflict.reported_by = ''
                                               THEN EXCLUDED.reported_by
                                               ELSE instrument_conflict.reported_by END",
                &[
                    &conflict.key(),
                    &identifiers,
                    &ids,
                    &conflict.reported_by,
                    &conflict.first_seen_ns,
                    &conflict.last_seen_ns,
                ],
            )
            .map_err(unavailable)?;
        Ok(())
    }

    fn conflicts(&self) -> Result<Vec<Conflict>> {
        let rows = self
            .conn()?
            .query(
                "SELECT identifiers, instrument_ids, reported_by, first_seen_ns, last_seen_ns
                   FROM instrument_conflict
                  ORDER BY first_seen_ns, conflict_key",
                &[],
            )
            .map_err(unavailable)?;
        Ok(rows
            .into_iter()
            .map(|row| {
                let identifiers: Vec<[String; 3]> =
                    serde_json::from_str(row.get::<_, &str>(0)).unwrap_or_default();
                Conflict {
                    identifiers: identifiers
                        .into_iter()
                        .map(|[scheme, source, value]| Asked {
                            scheme,
                            value,
                            source,
                        })
                        .collect(),
                    instrument_ids: serde_json::from_str(row.get::<_, &str>(1)).unwrap_or_default(),
                    reported_by: row.get(2),
                    first_seen_ns: row.get(3),
                    last_seen_ns: row.get(4),
                }
            })
            .collect())
    }

    fn replace(&self, replaced_id: &str, replaced_by: &str, now_ns: i64) -> Result<Replaced> {
        let mut conn = self.conn()?;

        // Conditional, so a redelivery and a race both land on the first
        // pairing: one statement, decided under the primary key.
        let recorded = conn
            .execute(
                "INSERT INTO instrument_replacement (replaced_id, replaced_by, replaced_at_ns)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (replaced_id) DO NOTHING",
                &[&replaced_id, &replaced_by, &now_ns],
            )
            .map_err(unavailable)?
            == 1;

        if recorded {
            return Ok(Replaced::Recorded);
        }

        let held: String = conn
            .query_one(
                "SELECT replaced_by FROM instrument_replacement WHERE replaced_id = $1",
                &[&replaced_id],
            )
            .map_err(unavailable)?
            .get(0);
        Ok(Replaced::AlreadyRecorded { replaced_by: held })
    }

    fn replacement_of(&self, instrument_id: &str) -> Result<Option<String>> {
        let row = self
            .conn()?
            .query_opt(
                "SELECT replaced_by FROM instrument_replacement WHERE replaced_id = $1",
                &[&instrument_id],
            )
            .map_err(unavailable)?;

        Ok(row.map(|row| row.get(0)))
    }
}

/// Every database failure is the instrument store being unavailable.
///
/// Not a loss of meaning: nothing above this trait can act differently on a
/// connection refused than on a syntax error, and a caller given the
/// distinction would only be tempted to.
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
