//! The instrument store's hot layer (step 3a; spec/one-data-access-model
//! section 3, "The hot layer"): records by ID and what each ID became --
//! `resolve-instrument`, asked up to eight times a page -- answered from the
//! store's own memory, Postgres the record.
//!
//! The writer holds the copy: every write passes through this store. A
//! record written or minted is evicted by its ID, and anything else written
//! -- a replacement, a venue, a calendar, a template, a company -- empties
//! the layer, rare beside the reads; an evicted record is read from the
//! record and held again. Bounded by `instrument.hotLayer.recordsMiB`, the
//! least recently read evicted first. A record by ID is the same at any
//! time asked (an ID is never reused), so a held one is always the record's
//! answer until a write evicts it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use prost::Message;

use crate::store::*;

/// The default bound: 32 MiB of records, within the store's 256 MiB limit.
pub const DEFAULT_BYTES: usize = 32 * 1024 * 1024;

/// The bound from the chart's value, MiB; the default where unset.
pub fn bytes_from_mib(said: Option<&str>) -> std::result::Result<usize, String> {
    match said {
        None | Some("") => Ok(DEFAULT_BYTES),
        Some(text) => text
            .parse::<usize>()
            .ok()
            .filter(|mib| *mib > 0 && *mib <= 4096)
            .map(|mib| mib * 1024 * 1024)
            .ok_or_else(|| {
                format!("instrument.hotLayer.recordsMiB is {text:?}; a whole number of MiB from 1 to 4096")
            }),
    }
}

#[derive(Default)]
struct Layer {
    records: BTreeMap<String, (Option<Instrument>, usize, u64)>,
    became: BTreeMap<String, (Option<String>, u64)>,
    by_use: BTreeMap<u64, (bool, String)>,
    bytes: usize,
    tick: u64,
    /// Moved by each eviction: a record read from the record while one was
    /// evicted is answered and not held, as it may be older than the write
    /// that evicted it.
    evicted: u64,
}

impl Layer {
    fn touch(&mut self, record: bool, id: &str) -> u64 {
        self.tick += 1;
        self.by_use.insert(self.tick, (record, id.to_string()));
        self.tick
    }

    fn trim(&mut self, bound: usize) {
        while self.bytes > bound {
            let Some((_, (record, id))) = self.by_use.pop_first() else {
                break;
            };
            self.forget(record, &id);
        }
    }

    fn forget(&mut self, record: bool, id: &str) {
        if record {
            if let Some((_, size, used)) = self.records.remove(id) {
                self.bytes -= size;
                self.by_use.remove(&used);
            }
        } else if let Some((_, used)) = self.became.remove(id) {
            self.bytes -= id.len() + 96;
            self.by_use.remove(&used);
        }
    }
}

/// The instrument store, with its hot layer in front.
pub struct HotStore {
    inner: Arc<dyn Store>,
    layer: Mutex<Layer>,
    bound: usize,
}

impl HotStore {
    pub fn new(inner: Arc<dyn Store>, bound: usize) -> Self {
        Self {
            inner,
            layer: Mutex::new(Layer::default()),
            bound,
        }
    }

    fn layer(&self) -> std::sync::MutexGuard<'_, Layer> {
        self.layer.lock().expect("the hot layer is poisoned")
    }

    /// How many records and replacements it holds, and their bytes.
    pub fn held(&self) -> (usize, usize, usize) {
        let layer = self.layer();
        (layer.records.len(), layer.became.len(), layer.bytes)
    }

    fn clear(&self) {
        let mut layer = self.layer();
        let evicted = layer.evicted + 1;
        *layer = Layer {
            evicted,
            ..Layer::default()
        };
    }

    fn evict(&self, id: &str) {
        let mut layer = self.layer();
        layer.forget(true, id);
        layer.forget(false, id);
        layer.evicted += 1;
    }

    /// A reader member's following (contract v30, stage B): any change the
    /// writer committed empties the layer, as any write but a record's own
    /// does on the writer; a record read again is held again.
    pub fn follow(&self) {
        self.clear();
    }
}

impl Store for HotStore {
    fn by_id(&self, instrument_id: &str) -> Result<Option<Instrument>> {
        {
            let mut layer = self.layer();
            if let Some((held, _, used)) = layer.records.get(instrument_id) {
                let (held, used) = (held.clone(), *used);
                layer.by_use.remove(&used);
                let tick = layer.touch(true, instrument_id);
                if let Some(entry) = layer.records.get_mut(instrument_id) {
                    entry.2 = tick;
                }
                return Ok(held);
            }
        }
        let before = self.layer().evicted;
        let read = self.inner.by_id(instrument_id)?;
        let size = read
            .as_ref()
            .map_or(0, |record| crate::record::to_wire(record).encoded_len() * 2)
            + instrument_id.len()
            + 160;
        let mut layer = self.layer();
        if layer.evicted != before {
            return Ok(read);
        }
        layer.forget(true, instrument_id);
        let tick = layer.touch(true, instrument_id);
        layer.bytes += size;
        layer
            .records
            .insert(instrument_id.to_string(), (read.clone(), size, tick));
        layer.trim(self.bound);
        Ok(read)
    }

    fn replacement_of(&self, instrument_id: &str) -> Result<Option<String>> {
        {
            let mut layer = self.layer();
            if let Some((held, used)) = layer.became.get(instrument_id) {
                let (held, used) = (held.clone(), *used);
                layer.by_use.remove(&used);
                let tick = layer.touch(false, instrument_id);
                if let Some(entry) = layer.became.get_mut(instrument_id) {
                    entry.1 = tick;
                }
                return Ok(held);
            }
        }
        let before = self.layer().evicted;
        let read = self.inner.replacement_of(instrument_id)?;
        let mut layer = self.layer();
        if layer.evicted != before {
            return Ok(read);
        }
        layer.forget(false, instrument_id);
        let tick = layer.touch(false, instrument_id);
        layer.bytes += instrument_id.len() + 96;
        layer
            .became
            .insert(instrument_id.to_string(), (read.clone(), tick));
        layer.trim(self.bound);
        Ok(read)
    }

    fn write(&self, record: Instrument, expected: i64, entry: Version) -> Result<Written> {
        let id = record.instrument_id.clone();
        let written = self.inner.write(record, expected, entry);
        self.evict(&id);
        written
    }

    fn mint(
        &self,
        candidate: Instrument,
        set_key: &str,
        first: Version,
    ) -> Result<(Instrument, Stood)> {
        let id = candidate.instrument_id.clone();
        let minted = self.inner.mint(candidate, set_key, first);
        self.evict(&id);
        if let Ok((held, _)) = &minted {
            self.evict(&held.instrument_id);
        }
        minted
    }

    fn windows(
        &self,
        scheme: &str,
        value: &str,
        source: &str,
    ) -> Result<Vec<(String, Identifier)>> {
        self.inner.windows(scheme, value, source)
    }

    fn matching(
        &self,
        scheme: &str,
        value: &str,
        source: &str,
        as_of_ns: i64,
    ) -> Result<Vec<Instrument>> {
        self.inner.matching(scheme, value, source, as_of_ns)
    }

    fn all(&self) -> Result<Vec<Instrument>> {
        self.inner.all()
    }

    fn each(&self, every: &mut dyn FnMut(Instrument) -> Result<bool>) -> Result<()> {
        self.inner.each(every)
    }

    fn count(&self) -> Result<usize> {
        self.inner.count()
    }

    fn history(&self, instrument_id: &str) -> Result<Vec<Version>> {
        self.inner.history(instrument_id)
    }

    fn note_conflict(&self, conflict: Conflict) -> Result<()> {
        let done = self.inner.note_conflict(conflict);
        self.clear();
        done
    }

    fn conflicts(&self) -> Result<Vec<Conflict>> {
        self.inner.conflicts()
    }

    fn replace(&self, replaced_id: &str, replaced_by: &str, now_ns: i64) -> Result<Replaced> {
        let done = self.inner.replace(replaced_id, replaced_by, now_ns);
        self.clear();
        done
    }

    fn replace_because(
        &self,
        replaced_id: &str,
        replaced_by: &str,
        cause: i32,
        now_ns: i64,
    ) -> Result<Replaced> {
        let done = self
            .inner
            .replace_because(replaced_id, replaced_by, cause, now_ns);
        self.clear();
        done
    }

    fn replacement_record(&self, replaced_id: &str) -> Result<Option<ReplacementKept>> {
        self.inner.replacement_record(replaced_id)
    }

    fn replacements_after(&self, sequence: i64, limit: usize) -> Result<Vec<ReplacementKept>> {
        self.inner.replacements_after(sequence, limit)
    }

    fn keep_template(
        &self,
        template: &meridian_domain::v1::SecurityTemplate,
        now_ns: i64,
    ) -> Result<bool> {
        let done = self.inner.keep_template(template, now_ns);
        self.clear();
        done
    }

    fn templates(&self) -> Result<Vec<meridian_domain::v1::SecurityTemplate>> {
        self.inner.templates()
    }

    fn keep_licence_list(
        &self,
        list: &meridian_domain::v1::LicenceList,
        now_ns: i64,
    ) -> Result<bool> {
        let done = self.inner.keep_licence_list(list, now_ns);
        self.clear();
        done
    }

    fn licence_list(&self) -> Result<Option<meridian_domain::v1::LicenceList>> {
        self.inner.licence_list()
    }

    fn state(&self, key: &str) -> Result<Option<String>> {
        self.inner.state(key)
    }

    fn set_state(&self, key: &str, value: &str) -> Result<()> {
        let done = self.inner.set_state(key, value);
        self.clear();
        done
    }

    fn keep_venue(&self, venue: &meridian_domain::v1::VenueRecord, now_ns: i64) -> Result<bool> {
        let done = self.inner.keep_venue(venue, now_ns);
        self.clear();
        done
    }

    fn venues(&self) -> Result<Vec<meridian_domain::v1::VenueRecord>> {
        self.inner.venues()
    }

    fn keep_calendar(
        &self,
        record: &meridian_domain::v1::CalendarRecord,
        now_ns: i64,
    ) -> Result<bool> {
        let done = self.inner.keep_calendar(record, now_ns);
        self.clear();
        done
    }

    fn keep_date_rule(
        &self,
        rule: &meridian_domain::v1::DateRuleVersion,
        now_ns: i64,
    ) -> Result<bool> {
        let done = self.inner.keep_date_rule(rule, now_ns);
        self.clear();
        done
    }

    fn calendar_versions(&self) -> Result<Vec<KeptCalendar>> {
        self.inner.calendar_versions()
    }

    fn date_rule_versions(&self) -> Result<Vec<KeptDateRule>> {
        self.inner.date_rule_versions()
    }

    fn keep_event_group(
        &self,
        group: &meridian_domain::v1::EventGroup,
        now_ns: i64,
    ) -> Result<Option<meridian_domain::v1::EventGroup>> {
        let done = self.inner.keep_event_group(group, now_ns);
        self.clear();
        done
    }

    fn event_groups(&self) -> Result<Vec<meridian_domain::v1::EventGroup>> {
        self.inner.event_groups()
    }

    fn keep_company(
        &self,
        company: &meridian_domain::v1::CompanyRecord,
        now_ns: i64,
    ) -> Result<bool> {
        let done = self.inner.keep_company(company, now_ns);
        self.clear();
        done
    }

    fn company(&self, company_id: &str) -> Result<Option<meridian_domain::v1::CompanyRecord>> {
        self.inner.company(company_id)
    }

    fn companies_with(
        &self,
        scheme: &str,
        value: &str,
    ) -> Result<Vec<meridian_domain::v1::CompanyRecord>> {
        self.inner.companies_with(scheme, value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryStore;

    fn record(instrument_id: &str, symbol: &str, version: i64) -> Instrument {
        let identifier = Identifier {
            scheme: "symbol".into(),
            value: symbol.into(),
            source: "snaptrade".into(),
            valid_from_ns: 0,
            valid_to_ns: None,
            trading_currency: String::new(),
        };
        Instrument {
            instrument_id: instrument_id.into(),
            identifiers: vec![identifier],
            description: format!("{symbol} v{version}"),
            lifecycle_state: "INSTRUMENT_LIFECYCLE_STATE_ACTIVE".into(),
            version,
            record_time_ns: 100,
            ..Default::default()
        }
    }

    fn entry(instrument_id: &str, version: i64) -> Version {
        Version {
            prepared_call_id: Default::default(),
            acting_through_delegation: String::new(),
            client_name: String::new(),
            instrument_id: instrument_id.into(),
            version,
            operation: "mint".into(),
            changes: Vec::new(),
            person: String::new(),
            instance_id: "custody-1".into(),
            note: String::new(),
            merged_instrument_id: String::new(),
            record_time_ns: 100 + version,
        }
    }

    fn minted(store: &dyn Store, n: usize) -> String {
        let id = format!("LCL-{n:04}");
        let key = crate::store::IdentifierSet::new([Asked {
            scheme: "symbol".into(),
            value: format!("S{n}"),
            source: "snaptrade".into(),
        }])
        .key();
        store
            .mint(record(&id, &format!("S{n}"), 1), &key, entry(&id, 1))
            .unwrap();
        id
    }

    /// Contract v30, stage B: a reader's layer is emptied by each change it
    /// follows, so a record the writer wrote is read again, and a reader
    /// writes nothing.
    #[test]
    fn a_readers_layer_follows_the_writers_changes_and_it_writes_nothing() {
        let primary: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let id = minted(primary.as_ref(), 1);
        let hot = Arc::new(HotStore::new(Arc::clone(&primary), DEFAULT_BYTES));
        let reader = crate::reader::ReaderStore::new(Arc::clone(&hot), Arc::clone(&primary), None);
        assert_eq!(reader.by_id(&id).unwrap().unwrap().version, 1);
        assert_eq!(hot.held().0, 1);
        // The writer writes; held until followed.
        primary
            .write(record(&id, "S1", 2), 1, entry(&id, 2))
            .unwrap();
        assert_eq!(reader.by_id(&id).unwrap().unwrap().version, 1);
        hot.follow();
        assert_eq!(hot.held().0, 0);
        assert_eq!(reader.by_id(&id).unwrap().unwrap().version, 2);
        assert!(reader
            .write(record(&id, "S1", 3), 2, entry(&id, 3))
            .is_err());
        assert!(reader.set_state("k", "v").is_err());
        assert_eq!(primary.by_id(&id).unwrap().unwrap().version, 2);
    }

    /// Contract v30, stage B (the resilience spec's section 5, approved
    /// 2026-11-10): a reader answers at the position it was answered at --
    /// its own applied position from its hot layer, the primary's past its
    /// wait. The instrument store's replies carry no store position of their
    /// own (no reference reply has a watermark; a list's `sequence` is the
    /// last item it read), so what a reply says is the record it answers.
    #[test]
    fn an_instrument_readers_answer_is_at_its_own_position_or_the_primarys_past_its_wait() {
        use meridian_bus::read_after::{Position, Reads};
        use meridian_pb::v1::{PartitionSequence, Watermark};
        struct At(u64);
        impl Position for At {
            fn heads(
                &self,
                _: &[String],
            ) -> std::result::Result<std::collections::HashMap<String, u64>, String> {
                Ok([("instruments".to_string(), self.0)].into())
            }
        }
        let primary: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let id = minted(primary.as_ref(), 1);
        let hot = Arc::new(HotStore::new(Arc::clone(&primary), DEFAULT_BYTES));
        let reader = crate::reader::ReaderStore::new(Arc::clone(&hot), Arc::clone(&primary), None);
        assert_eq!(reader.by_id(&id).unwrap().unwrap().version, 1);
        // The writer writes at 2; the reader has applied to 1.
        primary
            .write(record(&id, "S1", 2), 1, entry(&id, 2))
            .unwrap();
        let reads = Reads::default();
        reads.keep("reference", Arc::new(At(1)));
        reads.keep_primary("reference", Arc::new(At(2)));
        reads.set_bound(std::time::Duration::from_millis(20));
        let at = |sequence| Watermark {
            partitions: vec![PartitionSequence {
                partition: "instruments".into(),
                sequence,
            }],
        };
        let version = |asked: u64| {
            reads
                .answer(
                    "platform.reference.query.resolve-instrument",
                    &at(asked),
                    || reader.by_id(&id).map_err(|failed| failed.to_string()),
                )
                .unwrap()
                .unwrap()
                .version
        };
        assert_eq!(version(1), 1, "at its own position: its hot layer's");
        assert_eq!(
            version(2),
            2,
            "past its wait: the primary's, never its own behind it"
        );
    }

    /// Step 3a: the same answer as the record, before and after each write,
    /// under a bound small enough to evict.
    #[test]
    fn records_by_id_and_what_each_became_answer_as_the_record_does() {
        for bound in [DEFAULT_BYTES, 1_500] {
            let inner: Arc<dyn Store> = Arc::new(MemoryStore::new());
            let hot = HotStore::new(Arc::clone(&inner), bound);
            let ids: Vec<String> = (0..12).map(|n| minted(&hot, n)).collect();
            let same = |hot: &HotStore| {
                for id in ids.iter().chain(std::iter::once(&"LCL-NONE".to_string())) {
                    assert_eq!(hot.by_id(id).unwrap(), inner.by_id(id).unwrap(), "{id}");
                    assert_eq!(
                        crate::replace::current(hot, id).unwrap(),
                        crate::replace::current(inner.as_ref(), id).unwrap()
                    );
                }
            };
            same(&hot);
            same(&hot);
            assert!(hot.held().2 <= bound.max(1_500), "held past its bound");
            // A record written, one replaced by another: the next read answers it.
            let mut changed = hot.by_id(&ids[3]).unwrap().unwrap();
            changed.version = 2;
            changed.description = "renamed".into();
            assert_eq!(
                hot.write(changed, 1, entry(&ids[3], 2)).unwrap(),
                Written::Stored
            );
            hot.replace(&ids[5], &ids[6], 200).unwrap();
            same(&hot);
            assert_eq!(hot.by_id(&ids[3]).unwrap().unwrap().description, "renamed");
            assert_eq!(crate::replace::current(&hot, &ids[5]).unwrap(), ids[6]);
        }
    }
}
