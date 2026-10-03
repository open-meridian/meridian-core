//! The in-process store.
//!
//! The same contract as Postgres, for the tests that are about the domain
//! rather than the database. A deployment runs Postgres behind the same trait:
//! its records are its own (decisions/030), and nothing can send them again.

use std::sync::RwLock;

use crate::store::{
    Conflict, Held, Instrument, Replaced, Result, Stood, Store, StoreError, Version, Written,
};

/// The instrument store, held in memory.
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
            .map_err(|_| StoreError::Unavailable("the instrument store lock is poisoned".into()))
    }

    fn write_lock(&self) -> Result<std::sync::RwLockWriteGuard<'_, Held>> {
        self.held
            .write()
            .map_err(|_| StoreError::Unavailable("the instrument store lock is poisoned".into()))
    }
}

impl Store for MemoryStore {
    fn by_id(&self, instrument_id: &str) -> Result<Option<Instrument>> {
        Ok(self.read()?.by_id.get(instrument_id).cloned())
    }

    fn matching(
        &self,
        scheme: &str,
        value: &str,
        source: &str,
        as_of_ns: i64,
    ) -> Result<Vec<Instrument>> {
        Ok(self.read()?.matching(scheme, value, source, as_of_ns))
    }

    fn all(&self) -> Result<Vec<Instrument>> {
        Ok(self.read()?.all())
    }

    fn count(&self) -> Result<usize> {
        Ok(self.read()?.by_id.len())
    }

    fn mint(
        &self,
        candidate: Instrument,
        set_key: &str,
        first: Version,
    ) -> Result<(Instrument, Stood)> {
        Ok(self.write_lock()?.mint(candidate, set_key, first))
    }

    fn write(&self, record: Instrument, expected: i64, entry: Version) -> Result<Written> {
        Ok(self.write_lock()?.write(record, expected, entry))
    }

    fn history(&self, instrument_id: &str) -> Result<Vec<Version>> {
        Ok(self.read()?.history(instrument_id))
    }

    fn note_conflict(&self, conflict: Conflict) -> Result<()> {
        self.write_lock()?.note_conflict(conflict);
        Ok(())
    }

    fn conflicts(&self) -> Result<Vec<Conflict>> {
        Ok(self.read()?.conflicts())
    }

    fn replace(&self, replaced_id: &str, replaced_by: &str, now_ns: i64) -> Result<Replaced> {
        Ok(self.write_lock()?.replace(replaced_id, replaced_by, now_ns))
    }

    fn replacement_of(&self, instrument_id: &str) -> Result<Option<String>> {
        Ok(self
            .read()?
            .replacements
            .get(instrument_id)
            .map(|(replaced_by, _)| replaced_by.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Asked, Identifier, IdentifierSet};

    fn instrument(instrument_id: &str, version: i64) -> Instrument {
        Instrument {
            instrument_id: instrument_id.into(),
            identifiers: vec![Identifier {
                scheme: "figi".into(),
                value: "BBG000B9XRY4".into(),
                source: String::new(),
                valid_from_ns: 0,
                valid_to_ns: None,
            }],
            asset_class: String::new(),
            currency: String::new(),
            exchange_mic: String::new(),
            description: String::new(),
            lifecycle_state: String::new(),
            version,
            valid_from_ns: 0,
            record_time_ns: 0,
            instrument_type: String::new(),
            money_market_fund: String::new(),
            sources: Vec::new(),
            offers: Vec::new(),
        }
    }

    fn entry(instrument_id: &str, version: i64, operation: &str) -> Version {
        Version {
            acting_through_delegation: String::new(),
            client_name: String::new(),
            instrument_id: instrument_id.into(),
            version,
            operation: operation.into(),
            changes: Vec::new(),
            person: String::new(),
            instance_id: String::new(),
            note: String::new(),
            merged_instrument_id: String::new(),
            record_time_ns: version,
        }
    }

    fn key() -> String {
        IdentifierSet::new([Asked {
            scheme: "figi".into(),
            value: "BBG000B9XRY4".into(),
            source: String::new(),
        }])
        .key()
    }

    #[test]
    fn one_set_is_minted_once() {
        let store = MemoryStore::new();
        let (first, stood) = store
            .mint(instrument("LCL-1", 1), &key(), entry("LCL-1", 1, "mint"))
            .unwrap();
        assert_eq!(stood, Stood::Minted);
        let (second, stood) = store
            .mint(instrument("LCL-2", 1), &key(), entry("LCL-2", 1, "mint"))
            .unwrap();
        assert_eq!(stood, Stood::AlreadyHeld);
        assert_eq!(second.instrument_id, first.instrument_id);
        assert_eq!(store.count().unwrap(), 1);
    }

    #[test]
    fn a_write_against_a_version_since_moved_on_is_refused_and_writes_nothing() {
        let store = MemoryStore::new();
        store
            .mint(instrument("LCL-1", 1), &key(), entry("LCL-1", 1, "mint"))
            .unwrap();
        let mut next = instrument("LCL-1", 2);
        next.currency = "USD".into();
        assert_eq!(
            store
                .write(next.clone(), 1, entry("LCL-1", 2, "complete"))
                .unwrap(),
            Written::Stored
        );
        assert_eq!(
            store.write(next, 1, entry("LCL-1", 2, "complete")).unwrap(),
            Written::Stale { held: 2 }
        );
        assert_eq!(
            store
                .write(instrument("LCL-9", 2), 1, entry("LCL-9", 2, "complete"))
                .unwrap(),
            Written::Missing
        );
        let history = store.history("LCL-1").unwrap();
        assert_eq!(
            history.iter().map(|v| v.version).collect::<Vec<_>>(),
            vec![2, 1],
            "newest first, and nothing removed"
        );
    }
}
