//! The in-process street.
//!
//! What the unit tests run against, and what a single-host demo can run without
//! a database. Postgres sits behind the same trait; the rules live in `Held`,
//! which both implementations share, so neither can enforce a different set.

use std::collections::{BTreeMap, HashMap};
use std::sync::RwLock;

use crate::amounts::Quantity;
use crate::store::{
    activity_cursor, check_activity, connection, from_activity_cursor, from_statement_cursor,
    from_sync_status_cursor, statement_cursor, sync_status_cursor, ActivitiesRead, Activity,
    ActivityPage, Amended, Amendment, Cause, Chain, Change, Completed, Completion, Counts,
    CustodialPosition, Holding, Kept, Key, Opened, Page, Read, Result, Settled, Side, Statement,
    StatementPage, StatementsRead, Store, StoreError, SyncStatus, SyncStatusPage, SyncStatusesRead,
};

#[derive(Debug, Default)]
pub struct MemoryStore {
    held: RwLock<Held>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn read(&self) -> Result<std::sync::RwLockReadGuard<'_, Held>> {
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
    fn open(&self, statement: Statement, cause: &Cause) -> Result<(Statement, Opened, Completion)> {
        Ok(self.write()?.open(statement, cause))
    }

    fn statement(&self, statement_id: &str) -> Result<Option<Statement>> {
        Ok(self.read()?.statements.get(statement_id).cloned())
    }

    fn record(&self, holding: Holding, cause: &Cause) -> Result<(Settled, Completion)> {
        self.write()?.record(holding, cause)
    }

    fn counts(&self, statement_id: &str) -> Result<Counts> {
        self.read()?.counts(statement_id)
    }

    fn page(&self, read: &Read) -> Result<Page> {
        self.read()?.page(read)
    }

    fn statements(&self, read: &StatementsRead) -> Result<StatementPage> {
        self.read()?.statements_page(read)
    }

    fn custodial_position(
        &self,
        account_id: &str,
        instrument_id: &str,
        side: Side,
    ) -> Result<Option<CustodialPosition>> {
        Ok(self
            .read()?
            .positions
            .get(&Key {
                account_id: account_id.to_string(),
                instrument_id: instrument_id.to_string(),
                side,
            })
            .filter(|position| !position.removed)
            .cloned())
    }

    fn move_positions(
        &self,
        replaced_id: &str,
        instrument_id: &str,
        cause: &Cause,
    ) -> Result<Vec<Settled>> {
        Ok(self
            .write()?
            .move_positions(replaced_id, instrument_id, cause))
    }

    fn amend(&self, amendment: Amendment, cause: &Cause) -> Result<Amended> {
        self.write()?.amend(amendment, cause)
    }

    fn instruments_held(&self) -> Result<Vec<String>> {
        let held = self.read()?;
        let mut instruments: Vec<String> = held
            .positions
            .iter()
            .filter(|(_, position)| !position.removed)
            .map(|(key, _)| key.instrument_id.clone())
            .collect();
        instruments.sort();
        instruments.dedup();
        Ok(instruments)
    }

    fn record_activity(&self, activity: Activity, cause: &Cause) -> Result<(Activity, Kept)> {
        self.write()?.record_activity(activity, cause)
    }

    fn activities(&self, read: &ActivitiesRead) -> Result<ActivityPage> {
        self.read()?.activities_page(read)
    }

    fn record_sync_status(&self, status: SyncStatus, cause: &Cause) -> Result<SyncStatus> {
        Ok(self.write()?.record_sync_status(status, cause))
    }

    fn sync_statuses(&self, read: &SyncStatusesRead) -> Result<SyncStatusPage> {
        self.read()?.sync_statuses_page(read)
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

    /// Ordered by key, which is the order a read pages through them in. A
    /// removed one stays, a tombstone.
    pub(crate) positions: BTreeMap<Key, CustodialPosition>,

    /// The partition's last number, and the last of each kind for each
    /// account: what the next change takes and names as its previous.
    pub(crate) head: u64,
    pub(crate) chains: HashMap<(Chain, String), u64>,

    /// Each backfill journaled beside the row it amends (W2.4, contract v11):
    /// the row's identifier, the amendment and its cause, in the order they
    /// were journaled. The row in `holdings` stays as first recorded.
    pub(crate) amendments: Vec<(String, Amendment, Cause)>,

    /// Each activity in the order recorded (W2.10, contract v14), and its
    /// source, account and the custodian's identifier to its place there.
    pub(crate) activities: Vec<Activity>,
    pub(crate) activity_by_external: HashMap<(String, String, String), usize>,

    /// Each sync status heard, in the order recorded (W2.13).
    pub(crate) sync_statuses: Vec<SyncStatus>,
}

impl Held {
    /// The partition's next number, chained to the last of `chain` for the
    /// account. No holes: nothing here can roll back.
    fn next(&mut self, chain: Chain, account_id: &str) -> Change {
        self.head += 1;
        let previous = self
            .chains
            .insert((chain, account_id.to_string()), self.head)
            .unwrap_or(0);
        Change {
            sequence: self.head,
            previous,
        }
    }

    pub(crate) fn open(
        &mut self,
        statement: Statement,
        cause: &Cause,
    ) -> (Statement, Opened, Completion) {
        let external = (
            statement.source.clone(),
            statement.external_statement_id.clone(),
        );

        if let Some(existing) = self.by_external.get(&external) {
            if let Some(held) = self.statements.get(existing) {
                return (held.clone(), Opened::AlreadyRecorded, Completion::Nothing);
            }
        }

        let mut statement = statement;
        // Nothing is coming, so nothing is outstanding.
        let completion = if statement.expected_rows == 0 && statement.completed.is_none() {
            let change = self.next(Chain::Statement, &statement.account_id);
            statement.completed = Some(Completed {
                change,
                cause: cause.clone(),
            });
            Completion::JustCompleted(change)
        } else {
            Completion::Nothing
        };

        self.by_external
            .insert(external, statement.statement_id.clone());
        self.statements
            .insert(statement.statement_id.clone(), statement.clone());

        (statement, Opened::Opened, completion)
    }

    pub(crate) fn record(
        &mut self,
        holding: Holding,
        cause: &Cause,
    ) -> Result<(Settled, Completion)> {
        holding.validate()?;

        let Some(statement) = self.statements.get(&holding.statement_id).cloned() else {
            return Err(StoreError::UnknownStatement(holding.statement_id.clone()));
        };
        // A statement is one account's: one from a plugin before v7 takes its
        // first row's, and a row naming another is refused (W2.2).
        if statement.account_id.is_empty() {
            if let Some(held) = self.statements.get_mut(&holding.statement_id) {
                held.account_id = holding.account_id.clone();
            }
        } else if statement.account_id != holding.account_id {
            return Err(StoreError::AnotherAccount {
                row: holding.account_id.clone(),
                statement: statement.account_id.clone(),
            });
        }

        let settled = match holding.instrument_id.clone() {
            None => Settled::Unresolved,
            Some(instrument_id) => self.settle(&holding, instrument_id, cause)?,
        };

        let statement_id = holding.statement_id.clone();
        self.holdings.push(holding);

        let received = self
            .holdings
            .iter()
            .filter(|held| held.statement_id == statement_id)
            .count() as u32;

        // Once, at the row that reaches the count: a later row finds it
        // already completed.
        let held = self
            .statements
            .get(&statement_id)
            .cloned()
            .expect("checked above");
        let completion =
            if received >= held.expected_rows && held.expected_rows > 0 && held.completed.is_none()
            {
                let change = self.next(Chain::Statement, &held.account_id);
                if let Some(statement) = self.statements.get_mut(&statement_id) {
                    statement.completed = Some(Completed {
                        change,
                        cause: cause.clone(),
                    });
                }
                Completion::JustCompleted(change)
            } else {
                Completion::Nothing
            };

        Ok((settled, completion))
    }

    /// A holding row states a quantity as of a date, not a change to one, so
    /// the position is replaced rather than added to. Accumulating would double
    /// anything that appeared in two statements.
    fn settle(
        &mut self,
        holding: &Holding,
        instrument_id: String,
        cause: &Cause,
    ) -> Result<Settled> {
        let key = Key {
            account_id: holding.account_id.clone(),
            instrument_id: instrument_id.clone(),
            side: holding.side,
        };
        let statement = self
            .statements
            .get(&holding.statement_id)
            .expect("checked above");

        let previous = self.positions.get(&key).cloned();
        let previous_quantity = previous
            .as_ref()
            .map(|position| position.quantity)
            .unwrap_or(Quantity::ZERO);

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
            as_of_date: statement.as_of_date.clone(),
            updated_at_ns: cause.committed_at_ns,
            last_change: previous
                .as_ref()
                .map(|before| before.last_change)
                .unwrap_or_default(),
            removed: false,
        };

        let moved = match &previous {
            None => true,
            Some(before) => position.differs_from(before),
        };
        if moved {
            position.last_change = self.next(Chain::Position, &holding.account_id);
        }

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

    /// What a row carries now: as first recorded, with each backfill's field.
    pub(crate) fn as_amended(&self, holding: &Holding) -> crate::store::Cost {
        self.amendments
            .iter()
            .filter(|(holding_id, _, _)| *holding_id == holding.holding_id)
            .fold(holding.cost.clone(), |cost, (_, amendment, _)| {
                amendment.applied_to(&cost)
            })
    }

    pub(crate) fn amend(&mut self, amendment: Amendment, cause: &Cause) -> Result<Amended> {
        amendment.validate()?;
        if !self.statements.contains_key(&amendment.statement_id) {
            return Err(StoreError::UnknownStatement(amendment.statement_id.clone()));
        }
        let Some(row) = self
            .holdings
            .iter()
            .rev()
            .find(|held| {
                held.statement_id == amendment.statement_id
                    && held.account_id == amendment.account_id
                    && held.side == amendment.side
                    && held.instrument_id == amendment.instrument_id
                    && (amendment.instrument_id.is_some()
                        || held.unresolved_identifiers == amendment.unresolved_identifiers)
            })
            .cloned()
        else {
            return Err(StoreError::NoSuchRow(amendment.describes()));
        };
        let journaled = self.amendments.iter().any(|(holding_id, held, _)| {
            *holding_id == row.holding_id
                && held.contract_version == amendment.contract_version
                && held.field == amendment.field
        });
        let carried = self.as_amended(&row);
        if journaled || amendment.already_carried(&carried) {
            return Ok(Amended::Nothing);
        }
        let now = amendment.applied_to(&carried);
        self.amendments
            .push((row.holding_id.clone(), amendment, cause.clone()));

        let Some(instrument_id) = row.instrument_id.clone() else {
            return Ok(Amended::Amended(Settled::Unresolved));
        };
        let key = Key {
            account_id: row.account_id.clone(),
            instrument_id,
            side: row.side,
        };
        let Some(standing) = self.positions.get(&key).cloned() else {
            return Ok(Amended::Amended(Settled::Unresolved));
        };
        // Only the row that last stated the position speaks for it.
        if standing.removed || standing.last_statement_id != row.statement_id {
            return Ok(Amended::Amended(Settled::Unchanged { position: standing }));
        }
        let mut position = CustodialPosition {
            cost: now,
            updated_at_ns: cause.committed_at_ns,
            ..standing.clone()
        };
        if !position.differs_from(&standing) {
            return Ok(Amended::Amended(Settled::Unchanged { position: standing }));
        }
        position.last_change = self.next(Chain::Position, &row.account_id);
        self.positions.insert(key, position.clone());
        Ok(Amended::Amended(Settled::Changed {
            previous_quantity: standing.quantity,
            position,
        }))
    }

    pub(crate) fn move_positions(
        &mut self,
        replaced_id: &str,
        instrument_id: &str,
        cause: &Cause,
    ) -> Vec<Settled> {
        // In key order, which is account then side.
        let held: Vec<Key> = self
            .positions
            .iter()
            .filter(|(key, position)| key.instrument_id == replaced_id && !position.removed)
            .map(|(key, _)| key.clone())
            .collect();

        let mut settled = Vec::new();
        for placeholder_key in held {
            let Some(placeholder) = self.positions.get(&placeholder_key).cloned() else {
                continue;
            };
            let mut moved = CustodialPosition {
                instrument_id: instrument_id.to_string(),
                ..placeholder.clone()
            };
            let key = moved.key();

            match self.positions.get(&key).cloned() {
                Some(standing) if !standing.removed && !moved.stated_later_than(&standing) => {
                    // The one already under the instrument was stated later,
                    // so it stands and the placeholder's is gone.
                }
                standing => {
                    let changed = match &standing {
                        None => true,
                        Some(standing) => standing.removed || moved.differs_from(standing),
                    };
                    let previous_quantity = standing
                        .as_ref()
                        .filter(|standing| !standing.removed)
                        .map(|standing| standing.quantity)
                        .unwrap_or(Quantity::ZERO);
                    moved.updated_at_ns = cause.committed_at_ns;
                    if changed {
                        moved.last_change = self.next(Chain::Position, &moved.account_id);
                    } else if let Some(standing) = &standing {
                        moved.last_change = standing.last_change;
                    }
                    self.positions.insert(key, moved.clone());
                    settled.push(if changed {
                        Settled::Changed {
                            position: moved,
                            previous_quantity,
                        }
                    } else {
                        Settled::Unchanged { position: moved }
                    });
                }
            }

            // The placeholder's stays as a tombstone, numbered, so a reader
            // of changes learns it is gone (W2.6, W3.9).
            let mut removed = placeholder.tombstone();
            removed.updated_at_ns = cause.committed_at_ns;
            removed.last_change = self.next(Chain::Position, &removed.account_id);
            self.positions.insert(placeholder_key, removed.clone());
            settled.push(Settled::Changed {
                position: removed,
                previous_quantity: placeholder.quantity,
            });
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

    pub(crate) fn page(&self, read: &Read) -> Result<Page> {
        read.scope.admit(&read.account_id)?;
        let after = if read.cursor.is_empty() {
            None
        } else {
            Some(Key::from_cursor(&read.cursor)?)
        };

        // The map is in key order already; after the cursor's key, whole, so
        // a later account's rows are never skipped for sorting before it.
        let mut positions: Vec<CustodialPosition> = self
            .positions
            .iter()
            .filter(|(key, _)| after.as_ref().is_none_or(|after| *key > after))
            .map(|(_, position)| position)
            .filter(|position| read.scope.answers(&read.account_id, &position.account_id))
            .filter(|position| match read.since {
                None => !position.removed,
                Some(since) => position.last_change.sequence > since,
            })
            .take(read.limit + 1)
            .cloned()
            .collect();

        let more = positions.len() > read.limit;
        positions.truncate(read.limit);

        let next_cursor = match positions.last() {
            Some(last) if more => last.key().cursor(),
            _ => String::new(),
        };

        // Only when asked, and only with the first page. The reply's
        // postcondition says so, a reader who did not ask for gaps should not
        // be handed them silently, and one reading page by page should see
        // each once.
        let unresolved = if read.include_unresolved && after.is_none() {
            let mut rows: Vec<Holding> = self
                .holdings
                .iter()
                .filter(|holding| !holding.resolved())
                .filter(|holding| read.scope.answers(&read.account_id, &holding.account_id))
                .map(|holding| Holding {
                    cost: self.as_amended(holding),
                    ..holding.clone()
                })
                .collect();
            rows.sort_by(|left, right| left.holding_id.cmp(&right.holding_id));
            rows
        } else {
            Vec::new()
        };

        Ok(Page {
            positions,
            unresolved,
            next_cursor,
            as_of: self.head,
        })
    }

    pub(crate) fn statements_page(&self, read: &StatementsRead) -> Result<StatementPage> {
        read.scope.admit(&read.account_id)?;
        let after = if read.cursor.is_empty() {
            None
        } else {
            Some(from_statement_cursor(&read.cursor)?)
        };
        let mut completed: Vec<&Statement> = self
            .statements
            .values()
            .filter(|statement| statement.completed.is_some())
            .filter(|statement| read.scope.answers(&read.account_id, &statement.account_id))
            .filter(|statement| {
                read.as_of_date.is_empty() || statement.as_of_date == read.as_of_date
            })
            .filter(|statement| {
                let sequence = statement
                    .completed
                    .as_ref()
                    .map(|c| c.change.sequence)
                    .unwrap_or(0);
                read.since.is_none_or(|since| sequence > since)
            })
            .collect();
        completed.sort_by_key(|statement| {
            (
                statement
                    .completed
                    .as_ref()
                    .map(|c| c.change.sequence)
                    .unwrap_or(0),
                statement.statement_id.clone(),
            )
        });
        let mut page: Vec<&Statement> = completed
            .into_iter()
            .filter(|statement| {
                after.as_ref().is_none_or(|(sequence, statement_id)| {
                    let at = statement
                        .completed
                        .as_ref()
                        .map(|c| c.change.sequence)
                        .unwrap_or(0);
                    (at, &statement.statement_id) > (*sequence, statement_id)
                })
            })
            .take(read.limit + 1)
            .collect();
        let more = page.len() > read.limit;
        page.truncate(read.limit);
        let next_cursor = match page.last() {
            Some(last) if more => statement_cursor(last),
            _ => String::new(),
        };
        Ok(StatementPage {
            statements: page
                .into_iter()
                .map(|statement| Ok(((*statement).clone(), self.counts(&statement.statement_id)?)))
                .collect::<Result<_>>()?,
            next_cursor,
            as_of: self.head,
        })
    }
}

impl Held {
    pub(crate) fn record_activity(
        &mut self,
        mut activity: Activity,
        cause: &Cause,
    ) -> Result<(Activity, Kept)> {
        check_activity(&activity)?;
        let key = (
            activity.source.clone(),
            activity.account_id.clone(),
            activity.external_activity_id.clone(),
        );
        if let Some(&at) = self.activity_by_external.get(&key) {
            return Ok((self.activities[at].clone(), Kept::AlreadyRecorded));
        }
        let change = self.next(Chain::Activity, &activity.account_id);
        activity.recorded = Completed {
            change,
            cause: cause.clone(),
        };
        self.activity_by_external.insert(key, self.activities.len());
        self.activities.push(activity.clone());
        Ok((activity, Kept::Recorded))
    }

    pub(crate) fn activities_page(&self, read: &ActivitiesRead) -> Result<ActivityPage> {
        read.scope.admit(&read.account_id)?;
        let after = if read.cursor.is_empty() {
            None
        } else {
            Some(from_activity_cursor(&read.cursor)?)
        };
        let mut held: Vec<&Activity> = self
            .activities
            .iter()
            .filter(|activity| read.scope.answers(&read.account_id, &activity.account_id))
            .filter(|activity| {
                read.trade_date_from.is_empty() || activity.trade_date >= read.trade_date_from
            })
            .filter(|activity| {
                read.trade_date_to.is_empty() || activity.trade_date <= read.trade_date_to
            })
            .filter(|activity| {
                read.since
                    .is_none_or(|since| activity.recorded.change.sequence > since)
            })
            .collect();
        let by_record = read.since.is_some();
        let place = |activity: &Activity| {
            if by_record {
                (String::new(), activity.recorded.change.sequence)
            } else {
                (
                    activity.trade_date.clone(),
                    activity.recorded.change.sequence,
                )
            }
        };
        held.sort_by_key(|activity| place(activity));
        let mut page: Vec<&Activity> = held
            .into_iter()
            .filter(|activity| {
                after.as_ref().is_none_or(|(trade_date, sequence)| {
                    let after = if by_record {
                        (String::new(), *sequence)
                    } else {
                        (trade_date.clone(), *sequence)
                    };
                    place(activity) > after
                })
            })
            .take(read.limit + 1)
            .collect();
        let more = page.len() > read.limit;
        page.truncate(read.limit);
        let next_cursor = match page.last() {
            Some(last) if more => activity_cursor(last),
            _ => String::new(),
        };
        Ok(ActivityPage {
            activities: page.into_iter().cloned().collect(),
            next_cursor,
            as_of: self.head,
            history_from: self.history_from(&read.account_id),
        })
    }

    /// The named account's `history_from`, as its latest sync status said
    /// it; empty where none is named or none was heard.
    fn history_from(&self, account_id: &str) -> String {
        if account_id.is_empty() {
            return String::new();
        }
        self.sync_statuses
            .iter()
            .rev()
            .find(|status| status.account_id == account_id)
            .map(|status| status.history_from.clone())
            .unwrap_or_default()
    }

    pub(crate) fn record_sync_status(
        &mut self,
        mut status: SyncStatus,
        cause: &Cause,
    ) -> SyncStatus {
        let change = self.next(Chain::SyncStatus, &status.account_id);
        status.recorded = Completed {
            change,
            cause: cause.clone(),
        };
        self.sync_statuses.push(status.clone());
        status
    }

    pub(crate) fn sync_statuses_page(&self, read: &SyncStatusesRead) -> Result<SyncStatusPage> {
        read.scope.admit(&read.account_id)?;
        let after = if read.cursor.is_empty() {
            None
        } else {
            Some(from_sync_status_cursor(&read.cursor)?)
        };
        let answered =
            |status: &&SyncStatus| read.scope.answers(&read.account_id, &status.account_id);
        let mut page: Vec<&SyncStatus> = match read.since {
            Some(since) => self
                .sync_statuses
                .iter()
                .filter(answered)
                .filter(|status| status.recorded.change.sequence > since)
                .filter(|status| {
                    after
                        .as_ref()
                        .is_none_or(|(_, sequence)| status.recorded.change.sequence > *sequence)
                })
                .take(read.limit + 1)
                .collect(),
            None => {
                let mut latest: BTreeMap<(String, String, String), &SyncStatus> = BTreeMap::new();
                for status in self.sync_statuses.iter().filter(answered) {
                    latest.insert(connection(status), status);
                }
                latest
                    .into_iter()
                    .filter(|(held, _)| {
                        after
                            .as_ref()
                            .is_none_or(|(connection, _)| held > connection)
                    })
                    .map(|(_, status)| status)
                    .take(read.limit + 1)
                    .collect()
            }
        };
        let more = page.len() > read.limit;
        page.truncate(read.limit);
        let next_cursor = match page.last() {
            Some(last) if more => sync_status_cursor(last),
            _ => String::new(),
        };
        Ok(SyncStatusPage {
            statuses: page.into_iter().cloned().collect(),
            next_cursor,
            as_of: self.head,
        })
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
            .open(
                Statement {
                    statement_id: "STMT-1".into(),
                    source: "snaptrade".into(),
                    external_statement_id: "st-1".into(),
                    as_of_date: "2026-09-08".into(),
                    read_at_ns: 1,
                    expected_rows: 1,
                    account_id: String::new(),
                    external_account_id: String::new(),
                    institution: String::new(),
                    figures: Vec::new(),
                    currency_assumed: false,
                    security_interest: None,
                    raw_record: None,
                    provenance: Vec::new(),
                    completed: None,
                },
                &Cause::default(),
            )
            .unwrap();
        store
            .record(
                Holding {
                    holding_id: "HLD-1".into(),
                    statement_id: statement.statement_id,
                    account_id: "ACC-1".into(),
                    instrument_id: Some("LCL-1".into()),
                    unresolved_identifiers: vec![],
                    side: Side::Long,
                    quantity: "1".parse().unwrap(),
                    settle_date_quantity: None,
                    market_value: Some(Money::new("1".parse().unwrap(), "USD")),
                    currency_assumed: false,
                    also_counted_in_cash: false,
                    cost: Default::default(),
                    escalated: false,
                },
                &Cause::default(),
            )
            .unwrap();

        store
            .move_positions("LCL-1", "INS-1", &Cause::default())
            .unwrap();

        let held = store.read().unwrap();
        assert_eq!(held.holdings[0].instrument_id.as_deref(), Some("LCL-1"));
        assert!(held.positions.contains_key(&Key {
            account_id: "ACC-1".into(),
            instrument_id: "INS-1".into(),
            side: Side::Long,
        }));
    }
}
