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

    fn windows(
        &self,
        scheme: &str,
        value: &str,
        source: &str,
    ) -> Result<Vec<(String, crate::store::Identifier)>> {
        Ok(self.read()?.windows(scheme, value, source))
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

    fn replace_because(
        &self,
        replaced_id: &str,
        replaced_by: &str,
        cause: i32,
        now_ns: i64,
    ) -> Result<Replaced> {
        Ok(self
            .write_lock()?
            .replace(replaced_id, replaced_by, cause, now_ns))
    }

    fn replacement_record(
        &self,
        replaced_id: &str,
    ) -> Result<Option<crate::store::ReplacementKept>> {
        Ok(self
            .read()?
            .replacement_log
            .iter()
            .find(|kept| kept.replaced_id == replaced_id)
            .cloned())
    }

    fn replacements_after(
        &self,
        sequence: i64,
        limit: usize,
    ) -> Result<Vec<crate::store::ReplacementKept>> {
        Ok(self
            .read()?
            .replacement_log
            .iter()
            .filter(|kept| kept.sequence > sequence)
            .take(limit)
            .cloned()
            .collect())
    }

    fn keep_template(
        &self,
        template: &meridian_domain::v1::SecurityTemplate,
        _now_ns: i64,
    ) -> Result<bool> {
        let key = (
            template.template_id.clone(),
            template.template_version.clone(),
        );
        let mut held = self.write_lock()?;
        // A held copy that differs was decoded by an older build: replaced,
        // as the Postgres store does.
        if held.templates.get(&key) == Some(template) {
            return Ok(false);
        }
        held.templates.insert(key, template.clone());
        Ok(true)
    }

    fn templates(&self) -> Result<Vec<meridian_domain::v1::SecurityTemplate>> {
        Ok(self.read()?.templates.values().cloned().collect())
    }

    fn keep_licence_list(
        &self,
        list: &meridian_domain::v1::LicenceList,
        _now_ns: i64,
    ) -> Result<bool> {
        let mut held = self.write_lock()?;
        if held.licence_lists.contains_key(&list.version) {
            return Ok(false);
        }
        held.licence_lists.insert(list.version, list.clone());
        Ok(true)
    }

    fn licence_list(&self) -> Result<Option<meridian_domain::v1::LicenceList>> {
        Ok(self.read()?.licence_lists.values().last().cloned())
    }

    fn state(&self, key: &str) -> Result<Option<String>> {
        Ok(self.read()?.state.get(key).cloned())
    }

    fn set_state(&self, key: &str, value: &str) -> Result<()> {
        self.write_lock()?
            .state
            .insert(key.to_string(), value.to_string());
        Ok(())
    }

    fn replacement_of(&self, instrument_id: &str) -> Result<Option<String>> {
        Ok(self
            .read()?
            .replacements
            .get(instrument_id)
            .map(|(replaced_by, _)| replaced_by.clone()))
    }

    fn keep_venue(&self, venue: &meridian_domain::v1::VenueRecord, _now_ns: i64) -> Result<bool> {
        let mut held = self.write_lock()?;
        if held
            .venues
            .get(&venue.venue_id)
            .is_some_and(|kept| kept.version >= venue.version)
        {
            return Ok(false);
        }
        held.venues.insert(venue.venue_id.clone(), venue.clone());
        Ok(true)
    }

    fn venues(&self) -> Result<Vec<meridian_domain::v1::VenueRecord>> {
        Ok(self.read()?.venues.values().cloned().collect())
    }

    fn keep_event_group(
        &self,
        group: &meridian_domain::v1::EventGroup,
        now_ns: i64,
    ) -> Result<Option<meridian_domain::v1::EventGroup>> {
        let mut held = self.write_lock()?;
        if held.event_groups.iter().any(|kept| {
            kept.event_group_id == group.event_group_id && kept.version == group.version
        }) {
            return Ok(None);
        }
        let sequence = held
            .event_groups
            .last()
            .map(|kept| kept.sequence)
            .unwrap_or(0)
            + 1;
        let kept = meridian_domain::v1::EventGroup {
            record_time_ns: now_ns,
            sequence,
            ..group.clone()
        };
        held.event_groups.push(kept.clone());
        Ok(Some(kept))
    }

    fn event_groups(&self) -> Result<Vec<meridian_domain::v1::EventGroup>> {
        Ok(self.read()?.event_groups.clone())
    }

    fn keep_company(
        &self,
        company: &meridian_domain::v1::CompanyRecord,
        _now_ns: i64,
    ) -> Result<bool> {
        let mut held = self.write_lock()?;
        let key = (company.company_id.clone(), company.version);
        if held.companies.contains_key(&key) {
            return Ok(false);
        }
        held.companies.insert(key, company.clone());
        Ok(true)
    }

    fn company(&self, company_id: &str) -> Result<Option<meridian_domain::v1::CompanyRecord>> {
        Ok(self
            .read()?
            .companies
            .range((company_id.to_string(), i64::MIN)..=(company_id.to_string(), i64::MAX))
            .next_back()
            .map(|(_, record)| record.clone()))
    }

    fn companies_with(
        &self,
        scheme: &str,
        value: &str,
    ) -> Result<Vec<meridian_domain::v1::CompanyRecord>> {
        let held = self.read()?;
        let ids: std::collections::BTreeSet<&String> = held
            .companies
            .iter()
            .filter(|(_, record)| {
                record
                    .identifiers
                    .iter()
                    .any(|identifier| identifier.scheme == scheme && identifier.value == value)
            })
            .map(|((id, _), _)| id)
            .collect();
        let mut out = Vec::new();
        for id in ids {
            if let Some((_, record)) = held
                .companies
                .range((id.clone(), i64::MIN)..=(id.clone(), i64::MAX))
                .next_back()
            {
                out.push(record.clone());
            }
        }
        Ok(out)
    }

    fn keep_calendar(
        &self,
        record: &meridian_domain::v1::CalendarRecord,
        now_ns: i64,
    ) -> Result<bool> {
        let mut held = self.write_lock()?;
        let key = (record.calendar_id.clone(), record.version);
        if held.calendars.contains_key(&key) {
            return Ok(false);
        }
        held.calendars.insert(
            key,
            crate::store::KeptCalendar {
                record: record.clone(),
                kept_at_ns: now_ns,
            },
        );
        Ok(true)
    }

    fn keep_date_rule(
        &self,
        rule: &meridian_domain::v1::DateRuleVersion,
        now_ns: i64,
    ) -> Result<bool> {
        let mut held = self.write_lock()?;
        let key = (rule.kind, rule.subject.clone(), rule.version);
        if held.date_rules.contains_key(&key) {
            return Ok(false);
        }
        held.date_rules.insert(
            key,
            crate::store::KeptDateRule {
                rule: rule.clone(),
                kept_at_ns: now_ns,
            },
        );
        Ok(true)
    }

    fn calendar_versions(&self) -> Result<Vec<crate::store::KeptCalendar>> {
        Ok(self.read()?.calendars.values().cloned().collect())
    }

    fn date_rule_versions(&self) -> Result<Vec<crate::store::KeptDateRule>> {
        Ok(self.read()?.date_rules.values().cloned().collect())
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
                trading_currency: String::new(),
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
            binary_event_contract: String::new(),
            listed_terms: String::new(),
            listing_venue_id: String::new(),
            sources: Vec::new(),
            offers: Vec::new(),
            ..Default::default()
        }
    }

    fn entry(instrument_id: &str, version: i64, operation: &str) -> Version {
        Version {
            acting_through_delegation: String::new(),
            client_name: String::new(),
            prepared_call_id: String::new(),
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
