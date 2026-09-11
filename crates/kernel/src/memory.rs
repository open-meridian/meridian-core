//! The in-process ledger.
//!
//! What the unit tests run against, and what a single-host demo can run without
//! a database. Postgres sits behind the same trait; the rules live in `Held`,
//! which both implementations share, so neither can enforce a different set.

use std::collections::HashMap;
use std::sync::RwLock;

use crate::amounts::Quantity;
use crate::store::{
    Counts, Holding, Opened, Page, Position, Result, Settled, Statement, Store, StoreError,
};

#[derive(Debug, Default)]
pub struct MemoryStore {
    held: RwLock<Held>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn read(&self) -> Result<std::sync::RwLockReadGuard<'_, Held>> {
        self.held
            .read()
            .map_err(|_| StoreError::Unavailable("the ledger lock is poisoned".into()))
    }

    fn write(&self) -> Result<std::sync::RwLockWriteGuard<'_, Held>> {
        self.held
            .write()
            .map_err(|_| StoreError::Unavailable("the ledger lock is poisoned".into()))
    }
}

impl Store for MemoryStore {
    fn open(&self, statement: Statement) -> Result<(Statement, Opened)> {
        Ok(self.write()?.open(statement))
    }

    fn statement(&self, statement_id: &str) -> Result<Option<Statement>> {
        Ok(self.read()?.statements.get(statement_id).cloned())
    }

    fn record(&self, holding: Holding, now_ns: i64) -> Result<Settled> {
        self.write()?.record(holding, now_ns)
    }

    fn counts(&self, statement_id: &str) -> Result<Counts> {
        self.read()?.counts(statement_id)
    }

    fn page(
        &self,
        account_id: &str,
        include_unresolved: bool,
        limit: usize,
        cursor: &str,
    ) -> Result<Page> {
        Ok(self
            .read()?
            .page(account_id, include_unresolved, limit, cursor))
    }

    fn position(&self, account_id: &str, instrument_id: &str) -> Result<Option<Position>> {
        Ok(self
            .read()?
            .positions
            .get(&(account_id.to_string(), instrument_id.to_string()))
            .cloned())
    }
}

/// The ledger's contents, and every rule about them.
///
/// Shared so the Postgres implementation cannot enforce a different set. What
/// SQL does differently is where the rows live, not what may become one.
#[derive(Debug, Default)]
pub(crate) struct Held {
    pub(crate) statements: HashMap<String, Statement>,

    /// Source and the rail's own identifier to our own. What makes a
    /// redelivery recognisable.
    pub(crate) by_external: HashMap<(String, String), String>,

    pub(crate) holdings: Vec<Holding>,
    pub(crate) positions: HashMap<(String, String), Position>,
}

impl Held {
    pub(crate) fn open(&mut self, statement: Statement) -> (Statement, Opened) {
        let external = (
            statement.source.clone(),
            statement.external_statement_id.clone(),
        );

        if let Some(existing) = self.by_external.get(&external) {
            if let Some(held) = self.statements.get(existing) {
                return (held.clone(), Opened::AlreadyRecorded);
            }
        }

        self.by_external
            .insert(external, statement.statement_id.clone());
        self.statements
            .insert(statement.statement_id.clone(), statement.clone());

        (statement, Opened::Opened)
    }

    pub(crate) fn record(&mut self, holding: Holding, now_ns: i64) -> Result<Settled> {
        holding.validate()?;

        if !self.statements.contains_key(&holding.statement_id) {
            return Err(StoreError::UnknownStatement(holding.statement_id.clone()));
        }

        let settled = match holding.instrument_id.clone() {
            None => Settled::Unresolved,
            Some(instrument_id) => self.settle(&holding, instrument_id, now_ns)?,
        };

        self.holdings.push(holding);
        Ok(settled)
    }

    /// A holding row states a quantity as of a date, not a change to one, so
    /// the position is replaced rather than added to. Accumulating would double
    /// anything that appeared in two statements.
    fn settle(&mut self, holding: &Holding, instrument_id: String, now_ns: i64) -> Result<Settled> {
        let key = (holding.account_id.clone(), instrument_id.clone());
        let statement = self
            .statements
            .get(&holding.statement_id)
            .expect("checked above");

        let previous = self.positions.get(&key).cloned();
        let previous_quantity = previous
            .as_ref()
            .map(|position| position.quantity)
            .unwrap_or(Quantity::ZERO);

        let position = Position {
            account_id: holding.account_id.clone(),
            instrument_id,
            quantity: holding.quantity,
            market_value: holding.market_value,
            currency: holding.currency.clone(),
            last_statement_id: holding.statement_id.clone(),
            as_of_date: statement.as_of_date.clone(),
            updated_at_ns: now_ns,
        };

        let moved = match &previous {
            None => true,
            Some(before) => {
                before.quantity != position.quantity
                    || before.market_value != position.market_value
                    || before.currency != position.currency
            }
        };

        self.positions.insert(key, position.clone());

        Ok(if moved {
            Settled::Changed {
                position,
                previous_quantity,
            }
        } else {
            Settled::Unchanged { position }
        })
    }

    pub(crate) fn counts(&self, statement_id: &str) -> Result<Counts> {
        if !self.statements.contains_key(statement_id) {
            return Err(StoreError::UnknownStatement(statement_id.to_string()));
        }

        let mut counts = Counts::default();
        for holding in self
            .holdings
            .iter()
            .filter(|h| h.statement_id == statement_id)
        {
            counts.received += 1;
            if holding.resolved() {
                counts.resolved += 1;
            } else {
                counts.unresolved += 1;
            }
        }
        Ok(counts)
    }

    pub(crate) fn page(
        &self,
        account_id: &str,
        include_unresolved: bool,
        limit: usize,
        cursor: &str,
    ) -> Page {
        let mut positions: Vec<Position> = self
            .positions
            .values()
            .filter(|position| account_id.is_empty() || position.account_id == account_id)
            .filter(|position| cursor.is_empty() || position.instrument_id.as_str() > cursor)
            .cloned()
            .collect();

        positions.sort_by(|left, right| {
            (&left.account_id, &left.instrument_id).cmp(&(&right.account_id, &right.instrument_id))
        });

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

        // Only when asked. The reply's postcondition says so, and a reader who
        // did not ask for gaps should not be handed them silently.
        let unresolved = if include_unresolved {
            let mut rows: Vec<Holding> = self
                .holdings
                .iter()
                .filter(|holding| !holding.resolved())
                .filter(|holding| account_id.is_empty() || holding.account_id == account_id)
                .cloned()
                .collect();
            rows.sort_by(|left, right| left.holding_id.cmp(&right.holding_id));
            rows
        } else {
            Vec::new()
        };

        Page {
            positions,
            unresolved,
            next_cursor,
        }
    }
}
