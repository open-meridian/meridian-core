//! The in-process street.
//!
//! What the unit tests run against, and what a single-host demo can run without
//! a database. Postgres sits behind the same trait; the rules live in `Held`,
//! which both implementations share, so neither can enforce a different set.

use std::collections::HashMap;
use std::sync::RwLock;

use crate::amounts::Quantity;
use crate::store::{
    Completion, Counts, CustodialPosition, Holding, Opened, Page, Result, Settled, Statement,
    Store, StoreError,
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
            .map_err(|_| StoreError::Unavailable("the street store lock is poisoned".into()))
    }

    fn write(&self) -> Result<std::sync::RwLockWriteGuard<'_, Held>> {
        self.held
            .write()
            .map_err(|_| StoreError::Unavailable("the street store lock is poisoned".into()))
    }
}

impl Store for MemoryStore {
    fn open(&self, statement: Statement) -> Result<(Statement, Opened, Completion)> {
        Ok(self.write()?.open(statement))
    }

    fn statement(&self, statement_id: &str) -> Result<Option<Statement>> {
        Ok(self.read()?.statements.get(statement_id).cloned())
    }

    fn record(&self, holding: Holding, now_ns: i64) -> Result<(Settled, Completion)> {
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

    fn custodial_position(
        &self,
        account_id: &str,
        instrument_id: &str,
    ) -> Result<Option<CustodialPosition>> {
        Ok(self
            .read()?
            .positions
            .get(&(account_id.to_string(), instrument_id.to_string()))
            .cloned())
    }

    fn move_positions(&self, replaced_id: &str, instrument_id: &str) -> Result<Vec<Settled>> {
        Ok(self.write()?.move_positions(replaced_id, instrument_id))
    }

    fn placeholder_instruments(&self) -> Result<Vec<String>> {
        let held = self.read()?;
        let mut placeholders: Vec<String> = held
            .positions
            .keys()
            .map(|(_, instrument_id)| instrument_id)
            .filter(|instrument_id| instrument_id.starts_with(crate::ids::PLACEHOLDER_PREFIX))
            .cloned()
            .collect();
        placeholders.sort();
        placeholders.dedup();
        Ok(placeholders)
    }
}

/// The street store's contents, and every rule about them.
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
    pub(crate) positions: HashMap<(String, String), CustodialPosition>,

    /// Statements that have already announced themselves, so a row beyond the
    /// count does not announce a second time.
    pub(crate) completed: std::collections::HashSet<String>,
}

impl Held {
    pub(crate) fn open(&mut self, statement: Statement) -> (Statement, Opened, Completion) {
        let external = (
            statement.source.clone(),
            statement.external_statement_id.clone(),
        );

        if let Some(existing) = self.by_external.get(&external) {
            if let Some(held) = self.statements.get(existing) {
                return (held.clone(), Opened::AlreadyRecorded, Completion::Nothing);
            }
        }

        self.by_external
            .insert(external, statement.statement_id.clone());
        self.statements
            .insert(statement.statement_id.clone(), statement.clone());

        // Nothing is coming, so nothing is outstanding.
        let completion = if statement.expected_rows == 0
            && self.completed.insert(statement.statement_id.clone())
        {
            Completion::JustCompleted
        } else {
            Completion::Nothing
        };

        (statement, Opened::Opened, completion)
    }

    pub(crate) fn record(
        &mut self,
        holding: Holding,
        now_ns: i64,
    ) -> Result<(Settled, Completion)> {
        holding.validate()?;

        let Some(statement) = self.statements.get(&holding.statement_id).cloned() else {
            return Err(StoreError::UnknownStatement(holding.statement_id.clone()));
        };

        let settled = match holding.instrument_id.clone() {
            None => Settled::Unresolved,
            Some(instrument_id) => self.settle(&holding, instrument_id, now_ns)?,
        };

        let statement_id = holding.statement_id.clone();
        self.holdings.push(holding);

        let received = self
            .holdings
            .iter()
            .filter(|held| held.statement_id == statement_id)
            .count() as u32;

        // Once, at the row that reaches the count. `insert` returning true is
        // what makes it once: a later row finds it already there.
        let completion = if received >= statement.expected_rows
            && statement.expected_rows > 0
            && self.completed.insert(statement_id)
        {
            Completion::JustCompleted
        } else {
            Completion::Nothing
        };

        Ok((settled, completion))
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

        let position = CustodialPosition {
            account_id: holding.account_id.clone(),
            instrument_id,
            quantity: holding.quantity,
            market_value: holding.market_value.clone(),
            last_statement_id: holding.statement_id.clone(),
            as_of_date: statement.as_of_date.clone(),
            updated_at_ns: now_ns,
        };

        let moved = match &previous {
            None => true,
            Some(before) => position.differs_from(before),
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

    pub(crate) fn move_positions(
        &mut self,
        replaced_id: &str,
        instrument_id: &str,
    ) -> Vec<Settled> {
        let mut accounts: Vec<String> = self
            .positions
            .keys()
            .filter(|(_, held)| held == replaced_id)
            .map(|(account_id, _)| account_id.clone())
            .collect();
        accounts.sort();

        let mut settled = Vec::new();
        for account_id in accounts {
            let Some(placeholder) = self
                .positions
                .remove(&(account_id.clone(), replaced_id.to_string()))
            else {
                continue;
            };
            let moved = CustodialPosition {
                instrument_id: instrument_id.to_string(),
                ..placeholder
            };
            let key = (account_id, instrument_id.to_string());

            match self.positions.get(&key).cloned() {
                None => {
                    self.positions.insert(key, moved.clone());
                    settled.push(Settled::Changed {
                        position: moved,
                        previous_quantity: Quantity::ZERO,
                    });
                }
                Some(standing) if moved.stated_later_than(&standing) => {
                    self.positions.insert(key, moved.clone());
                    settled.push(if moved.differs_from(&standing) {
                        Settled::Changed {
                            position: moved,
                            previous_quantity: standing.quantity,
                        }
                    } else {
                        Settled::Unchanged { position: moved }
                    });
                }
                // The one already under the instrument was stated later, so it
                // stands and the placeholder's is gone.
                Some(_) => {}
            }
        }
        settled
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
        let mut positions: Vec<CustodialPosition> = self
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amounts::Money;

    #[test]
    fn a_placeholders_holding_rows_keep_the_placeholder_when_its_positions_move() {
        // The rows record what was reported, and what was reported named the
        // placeholder.
        let store = MemoryStore::new();
        let (statement, _, _) = store
            .open(Statement {
                statement_id: "STMT-1".into(),
                source: "snaptrade".into(),
                external_statement_id: "st-1".into(),
                as_of_date: "2026-09-08".into(),
                read_at_ns: 1,
                expected_rows: 1,
            })
            .unwrap();
        store
            .record(
                Holding {
                    holding_id: "HLD-1".into(),
                    statement_id: statement.statement_id,
                    account_id: "ACC-1".into(),
                    instrument_id: Some("LCL-1".into()),
                    unresolved_identifiers: vec![],
                    quantity: "1".parse().unwrap(),
                    market_value: Money::new("1".parse().unwrap(), "USD"),
                    escalated: false,
                },
                1,
            )
            .unwrap();

        store.move_positions("LCL-1", "INS-1").unwrap();

        let held = store.read().unwrap();
        assert_eq!(held.holdings[0].instrument_id.as_deref(), Some("LCL-1"));
        assert!(held
            .positions
            .contains_key(&("ACC-1".to_string(), "INS-1".to_string())));
    }
}
