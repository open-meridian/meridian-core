//! The in-process store.
//!
//! Not a stand-in for a first milestone. A deployment watching one brokerage
//! account holds a handful of instruments, and the instrument store is rebuilt from the
//! platform on demand, so durability buys less here than it does in the street store.
//! Its placeholders are the exception, being the deployment's own (see
//! `migrations/0002_placeholder.sql`), which is one reason a deployment runs
//! Postgres behind the same trait rather than this.

use std::sync::RwLock;

use crate::store::{
    Applied, Held, Instrument, Placeholder, Replaced, Result, Stood, Store, StoreError,
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

    fn write(&self) -> Result<std::sync::RwLockWriteGuard<'_, Held>> {
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

    fn stand_in(&self, candidate: Placeholder) -> Result<(Placeholder, Stood)> {
        Ok(self.write()?.stand_in(candidate))
    }

    fn placeholder(&self, placeholder_id: &str) -> Result<Option<Placeholder>> {
        Ok(self.read()?.placeholders.get(placeholder_id).cloned())
    }

    fn outstanding(&self) -> Result<Vec<Placeholder>> {
        Ok(self.read()?.outstanding())
    }

    fn legacy_outstanding(&self) -> Result<Vec<Instrument>> {
        Ok(self.read()?.legacy_outstanding())
    }

    fn replace(&self, replaced_id: &str, replaced_by: &str, now_ns: i64) -> Result<Replaced> {
        Ok(self.write()?.replace(replaced_id, replaced_by, now_ns))
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
    use crate::store::{Asked, IdentifierSet};

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

    fn asked(scheme: &str, value: &str, source: &str) -> Asked {
        Asked {
            scheme: scheme.into(),
            value: value.into(),
            source: source.into(),
        }
    }

    fn candidate(identifiers: Vec<Asked>) -> Placeholder {
        Placeholder {
            placeholder_id: crate::ids::placeholder(100),
            identifiers: IdentifierSet::new(identifiers),
            source: "snaptrade".into(),
            asset_class: String::new(),
            as_of_ns: 100,
            minted_at_ns: 100,
        }
    }

    #[test]
    fn one_set_in_any_order_is_one_placeholder() {
        // A connector describing one holding in a different order is still
        // describing one holding, and two placeholders for it would be two
        // positions for one security.
        let store = MemoryStore::new();
        let (first, stood) = store
            .stand_in(candidate(vec![
                asked("symbol", "ZZTOP", "snaptrade"),
                asked("figi", "BBG000ZZTOP1", ""),
            ]))
            .unwrap();
        assert_eq!(stood, Stood::Minted);

        let (again, stood) = store
            .stand_in(candidate(vec![
                asked("figi", "BBG000ZZTOP1", ""),
                asked("symbol", "ZZTOP", "snaptrade"),
                asked("figi", "BBG000ZZTOP1", ""),
            ]))
            .unwrap();
        assert_eq!(stood, Stood::AlreadyHeld);
        assert_eq!(again.placeholder_id, first.placeholder_id);
        assert_eq!(store.outstanding().unwrap().len(), 1);
    }

    #[test]
    fn a_replaced_placeholder_is_no_longer_outstanding_and_is_kept() {
        let store = MemoryStore::new();
        let (held, _) = store
            .stand_in(candidate(vec![asked("symbol", "ZZTOP", "snaptrade")]))
            .unwrap();

        assert_eq!(
            store
                .replace(&held.placeholder_id, "INS-ZZTOP", 200)
                .unwrap(),
            Replaced::Recorded
        );
        assert!(store.outstanding().unwrap().is_empty());

        // Never deleted: what a holding was recorded against stays readable.
        assert!(store.placeholder(&held.placeholder_id).unwrap().is_some());
        assert_eq!(
            store
                .replacement_of(&held.placeholder_id)
                .unwrap()
                .as_deref(),
            Some("INS-ZZTOP")
        );
    }

    #[test]
    fn a_second_replacement_changes_nothing() {
        let store = MemoryStore::new();
        store.replace("LCL-1", "INS-1", 200).unwrap();

        assert_eq!(
            store.replace("LCL-1", "INS-2", 300).unwrap(),
            Replaced::AlreadyRecorded {
                replaced_by: "INS-1".into()
            }
        );
        assert_eq!(
            store.replacement_of("LCL-1").unwrap().as_deref(),
            Some("INS-1")
        );
    }

    #[test]
    fn a_legacy_lcl_instrument_is_outstanding_until_replaced() {
        let store = MemoryStore::new();
        let mut legacy = instrument(1);
        legacy.instrument_id = "LCL-LEGACY".into();
        store.apply(legacy).unwrap();
        store.apply(instrument(1)).unwrap();

        let outstanding = store.legacy_outstanding().unwrap();
        assert_eq!(outstanding.len(), 1, "an INS- instrument is not legacy");
        assert_eq!(outstanding[0].instrument_id, "LCL-LEGACY");

        store.replace("LCL-LEGACY", "INS-1", 200).unwrap();
        assert!(store.legacy_outstanding().unwrap().is_empty());
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
