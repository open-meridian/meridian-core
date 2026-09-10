//! The in-process store.
//!
//! Not a placeholder for a first milestone. A deployment watching one brokerage
//! account holds a handful of instruments, and the replica is rebuilt from the
//! platform on demand, so durability buys less here than it does in the kernel.
//! Postgres arrives behind the same trait when compose does.

use std::sync::RwLock;

use crate::store::{Applied, Held, Instrument, Result, Store, StoreError};

/// A replica held in memory.
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
            .map_err(|_| StoreError::Unavailable("the replica lock is poisoned".into()))
    }

    fn write(&self) -> Result<std::sync::RwLockWriteGuard<'_, Held>> {
        self.held
            .write()
            .map_err(|_| StoreError::Unavailable("the replica lock is poisoned".into()))
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

    fn apply(&self, instrument: Instrument) -> Result<Applied> {
        Ok(self.write()?.apply(instrument))
    }

    fn version_of(&self, instrument_id: &str) -> Result<Option<i64>> {
        Ok(self
            .read()?
            .by_id
            .get(instrument_id)
            .map(|instrument| instrument.version))
    }

    fn count(&self) -> Result<usize> {
        Ok(self.read()?.by_id.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instrument(version: i64) -> Instrument {
        Instrument {
            instrument_id: "INS-1".into(),
            identifiers: vec![crate::store::Identifier {
                scheme: "figi".into(),
                value: "BBG000B9XRY4".into(),
                source: String::new(),
                valid_from_ns: 100,
                valid_to_ns: None,
            }],
            asset_class: "EQUITY".into(),
            currency: "USD".into(),
            exchange_mic: "XNAS".into(),
            description: "Apple Inc.".into(),
            lifecycle_state: "ACTIVE".into(),
            version,
            valid_from_ns: 100,
            record_time_ns: 100,
        }
    }

    #[test]
    fn an_applied_record_can_be_read_back() {
        let store = MemoryStore::new();
        assert_eq!(store.apply(instrument(1)).unwrap(), Applied::Stored);
        assert_eq!(store.by_id("INS-1").unwrap().unwrap().version, 1);
    }

    #[test]
    fn a_newer_version_replaces_an_older_one() {
        let store = MemoryStore::new();
        store.apply(instrument(1)).unwrap();
        assert_eq!(store.apply(instrument(4)).unwrap(), Applied::Stored);
        assert_eq!(store.version_of("INS-1").unwrap(), Some(4));
    }

    #[test]
    fn an_equal_version_is_already_current() {
        // The expected outcome of a retry after an ambiguous failure, which is
        // what lets the retry policy stay simple.
        let store = MemoryStore::new();
        store.apply(instrument(3)).unwrap();
        assert_eq!(store.apply(instrument(3)).unwrap(), Applied::AlreadyCurrent);
        assert_eq!(store.version_of("INS-1").unwrap(), Some(3));
    }

    #[test]
    fn an_older_version_does_not_overwrite_a_newer_one() {
        // Two pulls racing, or a redelivery arriving late. Neither may undo a
        // more recent record.
        let store = MemoryStore::new();
        store.apply(instrument(5)).unwrap();
        assert_eq!(store.apply(instrument(2)).unwrap(), Applied::AlreadyCurrent);
        assert_eq!(store.version_of("INS-1").unwrap(), Some(5));
        assert_eq!(
            store.by_id("INS-1").unwrap().unwrap().description,
            "Apple Inc."
        );
    }

    #[test]
    fn an_identifier_resolves_only_while_its_window_covers_the_moment() {
        let store = MemoryStore::new();
        let mut retired = instrument(2);
        retired.identifiers[0].valid_to_ns = Some(500);
        store.apply(retired).unwrap();

        // True at the time.
        assert_eq!(
            store
                .matching("figi", "BBG000B9XRY4", "", 300)
                .unwrap()
                .len(),
            1
        );

        // Not true afterwards. Deleting the mapping instead would make a
        // statement from before the change resolve to whoever holds that
        // identifier now.
        assert!(store
            .matching("figi", "BBG000B9XRY4", "", 900)
            .unwrap()
            .is_empty());

        // And not true before it began either.
        assert!(store
            .matching("figi", "BBG000B9XRY4", "", 50)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_scoped_identifier_does_not_answer_for_a_global_one() {
        // A brokerage symbol is meaningless outside its namespace.
        let store = MemoryStore::new();
        let mut scoped = instrument(1);
        scoped.identifiers[0].source = "snaptrade".into();
        store.apply(scoped).unwrap();

        assert!(store
            .matching("figi", "BBG000B9XRY4", "", 300)
            .unwrap()
            .is_empty());
        assert_eq!(
            store
                .matching("figi", "BBG000B9XRY4", "snaptrade", 300)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn the_resume_cursor_is_the_version_held_and_not_a_separate_marker() {
        let store = MemoryStore::new();
        assert_eq!(store.version_of("INS-1").unwrap(), None);
        store.apply(instrument(7)).unwrap();
        assert_eq!(store.version_of("INS-1").unwrap(), Some(7));
        assert_eq!(store.count().unwrap(), 1);
    }
}
