//! The lake's hot layer (step 3a; spec/one-data-access-model section 3, "The
//! hot layer"; plan Track 2): the reads a live page asks most, answered from
//! the lake's own memory, Postgres the record.
//!
//! - **Latest rows** per dataset, data type and subject: what the record
//!   answers a read of the latest in force now for that one subject, each
//!   conflation key's latest and latest not invalid. A read of several
//!   subjects is the union of theirs, which holds every row the record's own
//!   answer would (a store may answer more; the read chooses), and the read
//!   chooses from it as it chooses from the record's.
//! - **Listings**: a chain's series per underlying and dataset, as of now.
//! - **The catalogue's counts**, which `list-datasets` answers beside the
//!   configuration the lake already holds: from memory while no batch has
//!   changed a dataset since they were read, from the record otherwise.
//!
//! **The writer holds the copy**, so it is never behind its own commit: every
//! write passes through this store, and a batch's rows are applied to what
//! is held as it commits -- a new row key's row added, anything else that
//! could change what a key answers (a restatement, a rate, an assessment, a
//! closed-day amendment, a removal) evicting what it touches.
//!
//! **Same answer.** Only a read of the latest now (no `as_of_ns`, no valid
//! time, no published-time cut, not every version) or a listing as of now is
//! answered from memory, and only when every key it names is held; any
//! other read, and a read naming a key not yet held, goes to the record --
//! never partly from memory (DAM requirement 10).
//!
//! **Bounded.** Each layer's bound is a chart value (`lake.hotLayer.latestMiB`,
//! `lake.hotLayer.listingsMiB`); the least recently read keys are evicted
//! first, and an evicted key is answered from the record and loaded again.
//!
//! **Recovery from a watermark.** A key is loaded off the read's path: it is
//! marked loading, read from the record, and installed with every row the
//! writer committed for it while it loaded (what is above the record's
//! watermark, applied again where the record already held it, which changes
//! nothing). A load something evicted while it ran is not installed. A
//! restarted lake answers from the record until each key is loaded, then
//! from memory, equally.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex, Weak};

use meridian_changes::{ReadChangesReply, ReadError};
use meridian_domain::v1::{EntitlementsChangedEvent, SourcePriority};

use crate::row::{DataType, Observation};
use crate::store::{
    invalid, Assessment, ClosedDayAmendment, Listed, Query, Recorded, Removal, Result, Served,
    Stale, Store, WantChange, When,
};

/// The bounds of each layer, in bytes, from the chart's values.
#[derive(Debug, Clone, Copy)]
pub struct Bounds {
    pub latest_bytes: usize,
    pub listings_bytes: usize,
}

impl Bounds {
    /// The defaults, within the lake's 1 GiB (ruling 808), set from core's
    /// bench: 64 MiB of latest rows and 32 MiB of listings.
    pub const DEFAULT: Bounds = Bounds {
        latest_bytes: 64 * 1024 * 1024,
        listings_bytes: 32 * 1024 * 1024,
    };

    /// From the chart's values, MiB each; the default where one is unset.
    pub fn from_mib(
        latest: Option<&str>,
        listings: Option<&str>,
    ) -> std::result::Result<Self, String> {
        let mib = |said: Option<&str>, named: &str, default: usize| match said {
            None | Some("") => Ok(default),
            Some(text) => text
                .parse::<usize>()
                .ok()
                .filter(|mib| *mib > 0 && *mib <= 4096)
                .map(|mib| mib * 1024 * 1024)
                .ok_or_else(|| {
                    format!("{named} is {text:?}; a whole number of MiB from 1 to 4096")
                }),
        };
        Ok(Bounds {
            latest_bytes: mib(
                latest,
                "lake.hotLayer.latestMiB",
                Self::DEFAULT.latest_bytes,
            )?,
            listings_bytes: mib(
                listings,
                "lake.hotLayer.listingsMiB",
                Self::DEFAULT.listings_bytes,
            )?,
        })
    }
}

/// Which layer answered a read, for the read's timing line (`from_memory`,
/// `layer`): noted by the store on the thread that reads, each row read
/// from memory or the record, and taken by the line.
pub mod noted {
    use std::cell::Cell;

    thread_local! {
        static READS: Cell<(u32, u32, Option<&'static str>)> = const { Cell::new((0, 0, None)) };
    }

    /// A read begins: nothing noted yet.
    pub fn begin() {
        READS.with(|held| held.set((0, 0, None)));
    }

    /// Whether every row read this read made was answered from memory, and
    /// from which layer: `record` where any was not, empty where it read
    /// no rows.
    pub fn take() -> (bool, &'static str) {
        let (memory, record, layer) = READS.with(|held| held.replace((0, 0, None)));
        if record > 0 {
            (false, "record")
        } else {
            (memory > 0, layer.unwrap_or(""))
        }
    }

    /// What [`take`] would answer, leaving it to be taken: whether every
    /// row read so far came from memory.
    pub fn peek() -> (bool, &'static str) {
        let (memory, record, layer) = READS.with(|held| held.get());
        if record > 0 {
            (false, "record")
        } else {
            (memory > 0, layer.unwrap_or(""))
        }
    }

    pub(crate) fn memory(layer: &'static str) {
        READS.with(|held| {
            let (memory, record, _) = held.get();
            held.set((memory + 1, record, Some(layer)));
        });
    }

    pub(crate) fn record() {
        READS.with(|held| {
            let (memory, record, layer) = held.get();
            held.set((memory, record + 1, layer));
        });
    }
}

/// A key's rows held, least recently read evicted first past a bound.
struct Lru<K: Ord + Clone, V> {
    held: BTreeMap<K, (V, usize, u64)>,
    by_use: BTreeMap<u64, K>,
    bytes: usize,
    bound: usize,
    tick: u64,
}

impl<K: Ord + Clone, V> Lru<K, V> {
    fn new(bound: usize) -> Self {
        Self {
            held: BTreeMap::new(),
            by_use: BTreeMap::new(),
            bytes: 0,
            bound,
            tick: 0,
        }
    }

    fn get(&mut self, key: &K) -> Option<&V> {
        self.tick += 1;
        let tick = self.tick;
        let (_, _, used) = self.held.get_mut(key)?;
        self.by_use.remove(used);
        *used = tick;
        self.by_use.insert(tick, key.clone());
        self.held.get(key).map(|(value, _, _)| value)
    }

    fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        self.held.get_mut(key).map(|(value, _, _)| value)
    }

    fn contains(&self, key: &K) -> bool {
        self.held.contains_key(key)
    }

    fn put(&mut self, key: K, value: V, size: usize) {
        self.remove(&key);
        self.tick += 1;
        self.bytes += size;
        self.by_use.insert(self.tick, key.clone());
        self.held.insert(key, (value, size, self.tick));
        self.trim();
    }

    /// A held value's size changed.
    fn resize(&mut self, key: &K, size: usize) {
        if let Some((_, held, _)) = self.held.get_mut(key) {
            self.bytes = self.bytes - *held + size;
            *held = size;
        }
        self.trim();
    }

    fn remove(&mut self, key: &K) {
        if let Some((_, size, used)) = self.held.remove(key) {
            self.bytes -= size;
            self.by_use.remove(&used);
        }
    }

    fn retain(&mut self, mut keep: impl FnMut(&K) -> bool) {
        let gone: Vec<K> = self.held.keys().filter(|k| !keep(k)).cloned().collect();
        for key in gone {
            self.remove(&key);
        }
    }

    fn trim(&mut self) {
        while self.bytes > self.bound {
            let Some((_, key)) = self.by_use.pop_first() else {
                break;
            };
            if let Some((_, size, _)) = self.held.remove(&key) {
                self.bytes -= size;
            }
        }
    }

    fn len(&self) -> usize {
        self.held.len()
    }
}

/// A latest key: dataset, data type and subject.
type LatestKey = (String, i16, String);
/// A listing key: dataset and underlying.
type ListingKey = (String, String);

/// A key being loaded: the rows committed for it since it was marked, and
/// whether something evicted it meanwhile.
#[derive(Default)]
struct Loading {
    rows: Vec<Observation>,
    spoiled: bool,
}

#[derive(Default)]
struct Counts {
    unconverted: Option<BTreeMap<String, u64>>,
    closed_day: Option<BTreeMap<String, (u64, u64)>>,
    quality: Option<BTreeMap<String, (u64, u64, u64)>>,
}

struct Layer {
    latest: Lru<LatestKey, Held>,
    listings: Lru<ListingKey, Vec<Listed>>,
    loading_latest: HashMap<LatestKey, Loading>,
    loading_listings: HashMap<ListingKey, Loading>,
    /// Keys asked and not held, waiting for the loader.
    wanted_latest: BTreeSet<LatestKey>,
    wanted_listings: BTreeSet<ListingKey>,
    counts: Counts,
}

/// What a row costs held, roughly: its slot, its boxed message, and its
/// strings and lists at three bytes of heap a byte encoded (as the lake's
/// held answers count them, pages.rs).
fn cost(row: &Observation) -> usize {
    std::mem::size_of::<Observation>() + row.boxed_bytes() + row.encoded_len() * 3
}

fn cost_of(rows: &[Observation]) -> usize {
    rows.iter().map(cost).sum::<usize>() + 96
}

fn listed_cost(listed: &[Listed]) -> usize {
    listed
        .iter()
        .map(|l| l.dataset.len() + l.series.len() + 64)
        .sum::<usize>()
        + 96
}

/// A row's conflation key within its subject's latest: what the read
/// chooses one latest and one latest not invalid of. Kept finer than the
/// record's and the read's own, so what is pruned is never one either would
/// choose.
fn conflation(row: &Observation) -> (String, i32, String, i64, String, String, String) {
    let keyed = row.latest_key();
    let key = if keyed.is_empty() {
        format!("row:{}", row.meta().row_key)
    } else {
        keyed
    };
    (
        row.subject().to_string(),
        row.kind(),
        row.venue().to_string(),
        row.interval_ns(),
        key,
        row.asset(),
        row.point(),
    )
}

/// Each conflation key's latest and latest not invalid, by valid time then
/// sequence; every other row of a key dropped. A row key with no
/// `latest_key` is a key of its own, as in the record.
fn pruned(rows: Vec<Observation>) -> Vec<Observation> {
    let order = |row: &Observation| (row.meta().valid_from_ns, row.meta().sequence);
    let mut latest: BTreeMap<_, usize> = BTreeMap::new();
    let mut valid: BTreeMap<_, usize> = BTreeMap::new();
    for (at, row) in rows.iter().enumerate() {
        let key = conflation(row);
        if latest
            .get(&key)
            .is_none_or(|held: &usize| order(&rows[*held]) < order(row))
        {
            latest.insert(key.clone(), at);
        }
        if !invalid(row)
            && valid
                .get(&key)
                .is_none_or(|held: &usize| order(&rows[*held]) < order(row))
        {
            valid.insert(key, at);
        }
    }
    let kept: BTreeSet<usize> = latest.into_values().chain(valid.into_values()).collect();
    let mut out: Vec<Observation> = rows
        .into_iter()
        .enumerate()
        .filter(|(at, _)| kept.contains(at))
        .map(|(_, row)| row)
        .collect();
    out.sort_by_key(|row| row.meta().sequence);
    out
}

/// Whether a read is one memory may answer: the latest in force at the
/// latest valid time, as recorded by now or by a cut-off at or after every
/// row each key held reflects (checked key by key).
fn answers_latest(query: &Query) -> bool {
    query.latest_only
        && matches!(query.when, When::Latest { at_ns: 0 })
        && query.published_as_of_ns == 0
        && !crate::store::every_version(query)
}

/// A key's latest rows, and the latest time any row it has seen was
/// recorded: a read cut off before it is the record's to answer.
#[derive(Default)]
struct Held {
    rows: Vec<Observation>,
    newest_ns: i64,
}

fn newest(rows: &[Observation]) -> i64 {
    rows.iter()
        .map(|row| row.meta().recorded_at_ns)
        .max()
        .unwrap_or(0)
}

/// Whether a committed row can be added to what a key holds without asking
/// the record again: a new row key, of a data type in force by version.
fn appendable(row: &Observation) -> bool {
    row.meta().version == 1 && row.data_type() != DataType::Rate
}

/// The lake's store, with its hot layer in front.
pub struct HotStore {
    inner: Arc<dyn Store>,
    layer: Mutex<Layer>,
    /// The loader waits here for keys wanted.
    wake: Condvar,
}

impl HotStore {
    /// The store, its hot layer bounded by `bounds`, and a loader thread
    /// that loads each key asked and not held.
    pub fn new(inner: Arc<dyn Store>, bounds: Bounds) -> Arc<Self> {
        let hot = Arc::new(Self {
            inner,
            layer: Mutex::new(Layer {
                latest: Lru::new(bounds.latest_bytes),
                listings: Lru::new(bounds.listings_bytes),
                loading_latest: HashMap::new(),
                loading_listings: HashMap::new(),
                wanted_latest: BTreeSet::new(),
                wanted_listings: BTreeSet::new(),
                counts: Counts::default(),
            }),
            wake: Condvar::new(),
        });
        let held = Arc::downgrade(&hot);
        std::thread::Builder::new()
            .name("lake-hot-layer".into())
            .spawn(move || Self::load_forever(held))
            .expect("a thread for the hot layer's loader");
        hot
    }

    fn layer(&self) -> std::sync::MutexGuard<'_, Layer> {
        self.layer.lock().expect("the hot layer is poisoned")
    }

    /// What each layer holds: keys and bytes, for the bench and the tests.
    pub fn held(&self) -> ((usize, usize), (usize, usize)) {
        let layer = self.layer();
        (
            (layer.latest.len(), layer.latest.bytes),
            (layer.listings.len(), layer.listings.bytes),
        )
    }

    /// Load every key wanted now, on this thread: what the loader does, for
    /// a test that waits for it.
    pub fn load_wanted(&self) {
        loop {
            if self.load_one() {
                continue;
            }
            // The loader's own thread may hold one still loading.
            let busy = {
                let layer = self.layer();
                !layer.loading_latest.is_empty() || !layer.loading_listings.is_empty()
            };
            if !busy {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    fn load_forever(held: Weak<HotStore>) {
        loop {
            let Some(hot) = held.upgrade() else {
                return;
            };
            if !hot.load_one() {
                let layer = hot.layer();
                if layer.wanted_latest.is_empty() && layer.wanted_listings.is_empty() {
                    let _ = hot
                        .wake
                        .wait_timeout(layer, std::time::Duration::from_secs(1))
                        .expect("the hot layer is poisoned");
                }
            }
        }
    }

    /// Load one wanted key; false when none is wanted.
    fn load_one(&self) -> bool {
        let next = {
            let mut layer = self.layer();
            if let Some(key) = layer.wanted_latest.pop_first() {
                if layer.latest.contains(&key) || layer.loading_latest.contains_key(&key) {
                    return true;
                }
                layer.loading_latest.insert(key.clone(), Loading::default());
                Ok(key)
            } else if let Some(key) = layer.wanted_listings.pop_first() {
                if layer.listings.contains(&key) || layer.loading_listings.contains_key(&key) {
                    return true;
                }
                layer
                    .loading_listings
                    .insert(key.clone(), Loading::default());
                Err(key)
            } else {
                return false;
            }
        };
        match next {
            Ok(key) => self.load_latest(key),
            Err(key) => self.load_listing(key),
        }
        true
    }

    fn load_latest(&self, key: LatestKey) {
        let (dataset, code, subject) = key.clone();
        let read = DataType::from_code(code).ok_or(()).and_then(|data_type| {
            self.inner
                .read(&Query {
                    datasets: vec![dataset],
                    subjects: vec![subject],
                    data_type,
                    kinds: Vec::new(),
                    interval_ns: 0,
                    when: When::Latest { at_ns: 0 },
                    as_of_ns: 0,
                    published_as_of_ns: 0,
                    latest_only: true,
                })
                .map_err(|_| ())
        });
        let mut layer = self.layer();
        let Some(loading) = layer.loading_latest.remove(&key) else {
            return;
        };
        let Ok(mut rows) = read else {
            return;
        };
        if loading.spoiled {
            return;
        }
        // What the writer committed while it loaded, applied again where
        // the record already held it.
        let held: HashSet<u64> = rows.iter().map(|row| row.meta().sequence).collect();
        rows.extend(
            loading
                .rows
                .into_iter()
                .filter(|row| !held.contains(&row.meta().sequence)),
        );
        let newest_ns = newest(&rows);
        let rows = pruned(rows);
        let size = cost_of(&rows);
        layer.latest.put(key, Held { rows, newest_ns }, size);
    }

    fn load_listing(&self, key: ListingKey) {
        let (dataset, subject) = key.clone();
        let read = self.inner.listing(&[dataset], &[subject], 0);
        let mut layer = self.layer();
        let Some(loading) = layer.loading_listings.remove(&key) else {
            return;
        };
        let Ok(listed) = read else {
            return;
        };
        if loading.spoiled || !loading.rows.is_empty() {
            // A series committed while it loaded: loaded again when asked.
            return;
        }
        let size = listed_cost(&listed);
        layer.listings.put(key, listed, size);
    }

    /// A batch committed: its rows applied to what each key holds.
    fn committed(&self, dataset: &str, rows: &[Observation]) {
        let mut layer = self.layer();
        if !rows.is_empty() {
            layer.counts = Counts::default();
        }
        for row in rows {
            let code = row.data_type().code();
            for subject in row.subjects() {
                let key = (dataset.to_string(), code, subject.clone());
                if let Some(loading) = layer.loading_latest.get_mut(&key) {
                    if appendable(row) {
                        loading.rows.push(row.clone());
                    } else {
                        loading.spoiled = true;
                    }
                }
                if !appendable(row) {
                    layer.latest.remove(&key);
                    continue;
                }
                let resized = layer.latest.get_mut(&key).map(|held| {
                    let mut rows = std::mem::take(&mut held.rows);
                    rows.push(row.clone());
                    held.rows = pruned(rows);
                    held.newest_ns = held.newest_ns.max(row.meta().recorded_at_ns);
                    cost_of(&held.rows)
                });
                if let Some(size) = resized {
                    layer.latest.resize(&key, size);
                }
            }
            if row.data_type() == DataType::Series {
                let key = (dataset.to_string(), row.subject().to_string());
                layer.listings.remove(&key);
                if let Some(loading) = layer.loading_listings.get_mut(&key) {
                    loading.rows.push(row.clone());
                }
            }
        }
    }

    /// Something changed what a dataset's keys answer that is not a row
    /// added: everything held of it evicted, every load of it spoiled.
    fn evict_dataset(&self, dataset: &str) {
        let mut layer = self.layer();
        layer.latest.retain(|(held, _, _)| held != dataset);
        layer.listings.retain(|(held, _)| held != dataset);
        for (key, loading) in layer.loading_latest.iter_mut() {
            if key.0 == dataset {
                loading.spoiled = true;
            }
        }
        for (key, loading) in layer.loading_listings.iter_mut() {
            if key.0 == dataset {
                loading.spoiled = true;
            }
        }
        layer.counts = Counts::default();
    }

    /// The latest now, from memory where every key asked is held.
    fn latest_from_memory(&self, query: &Query) -> Option<Vec<Observation>> {
        let code = query.data_type.code();
        let mut layer = self.layer();
        let subjects: BTreeSet<&String> = query.subjects.iter().collect();
        let datasets: BTreeSet<&String> = query.datasets.iter().collect();
        let mut missing = Vec::new();
        let mut out = Vec::new();
        let mut seen: HashSet<(String, u64)> = HashSet::new();
        for dataset in &datasets {
            for subject in &subjects {
                let key = ((*dataset).clone(), code, (*subject).clone());
                match layer.latest.get(&key) {
                    // Cut off before what the key has seen: the record's.
                    Some(held) if query.as_of_ns != 0 && held.newest_ns > query.as_of_ns => {
                        return None;
                    }
                    Some(held) => {
                        for row in &held.rows {
                            let wanted = (query.kinds.is_empty()
                                || query.kinds.contains(&row.kind()))
                                && (query.interval_ns == 0
                                    || row.interval_ns() == query.interval_ns);
                            if wanted
                                && seen.insert((row.dataset().to_string(), row.meta().sequence))
                            {
                                out.push(row.clone());
                            }
                        }
                    }
                    None => missing.push(key),
                }
            }
        }
        if !missing.is_empty() {
            layer.wanted_latest.extend(missing);
            self.wake.notify_one();
            return None;
        }
        out.sort_by(|a, b| (a.dataset(), a.meta().sequence).cmp(&(b.dataset(), b.meta().sequence)));
        Some(out)
    }

    fn listing_from_memory(&self, datasets: &[String], subjects: &[String]) -> Option<Vec<Listed>> {
        let mut layer = self.layer();
        let subjects: BTreeSet<&String> = subjects.iter().collect();
        let datasets: BTreeSet<&String> = datasets.iter().collect();
        let mut missing = Vec::new();
        let mut out = Vec::new();
        for dataset in &datasets {
            for subject in &subjects {
                let key = ((*dataset).clone(), (*subject).clone());
                match layer.listings.get(&key) {
                    Some(listed) => out.extend(listed.iter().cloned()),
                    None => missing.push(key),
                }
            }
        }
        if !missing.is_empty() {
            layer.wanted_listings.extend(missing);
            self.wake.notify_one();
            return None;
        }
        Some(out)
    }
}

impl Store for HotStore {
    fn record(&self, dataset: &str, rows: Vec<Observation>, now_ns: i64) -> Result<Recorded> {
        let done = self.inner.record(dataset, rows, now_ns)?;
        self.committed(dataset, &done.rows);
        Ok(done)
    }

    fn serve_unkept(
        &self,
        dataset: &str,
        rows: Vec<Observation>,
        readers: &[String],
        now_ns: i64,
    ) -> Result<Recorded> {
        self.inner.serve_unkept(dataset, rows, readers, now_ns)
    }

    fn read(&self, query: &Query) -> Result<Vec<Observation>> {
        if answers_latest(query) {
            if let Some(rows) = self.latest_from_memory(query) {
                noted::memory("latest");
                return Ok(rows);
            }
        }
        noted::record();
        self.inner.read(query)
    }

    fn listing(
        &self,
        datasets: &[String],
        subjects: &[String],
        as_of_ns: i64,
    ) -> Result<Vec<Listed>> {
        if as_of_ns == 0 {
            if let Some(listed) = self.listing_from_memory(datasets, subjects) {
                noted::memory("listings");
                return Ok(listed);
            }
        }
        noted::record();
        self.inner.listing(datasets, subjects, as_of_ns)
    }

    fn unconverted_counts(&self) -> Result<BTreeMap<String, u64>> {
        if let Some(held) = self.layer().counts.unconverted.clone() {
            noted::memory("catalogue");
            return Ok(held);
        }
        noted::record();
        let read = self.inner.unconverted_counts()?;
        self.layer().counts.unconverted = Some(read.clone());
        Ok(read)
    }

    fn remove_before(
        &self,
        dataset: &str,
        recorded_before_ns: i64,
        why: &str,
        now_ns: i64,
    ) -> Result<u64> {
        let removed = self
            .inner
            .remove_before(dataset, recorded_before_ns, why, now_ns);
        self.evict_dataset(dataset);
        removed
    }

    fn amend_closed_day(&self, amendments: &[ClosedDayAmendment]) -> Result<()> {
        let amended = self.inner.amend_closed_day(amendments);
        let datasets: BTreeSet<&str> = amendments.iter().map(|a| a.dataset.as_str()).collect();
        for dataset in datasets {
            self.evict_dataset(dataset);
        }
        amended
    }

    fn closed_day_counts(&self) -> Result<BTreeMap<String, (u64, u64)>> {
        if let Some(held) = self.layer().counts.closed_day.clone() {
            noted::memory("catalogue");
            return Ok(held);
        }
        noted::record();
        let read = self.inner.closed_day_counts()?;
        self.layer().counts.closed_day = Some(read.clone());
        Ok(read)
    }

    fn quality_counts(&self) -> Result<BTreeMap<String, (u64, u64, u64)>> {
        if let Some(held) = self.layer().counts.quality.clone() {
            noted::memory("catalogue");
            return Ok(held);
        }
        noted::record();
        let read = self.inner.quality_counts()?;
        self.layer().counts.quality = Some(read.clone());
        Ok(read)
    }

    fn assess(&self, assessments: &[Assessment]) -> Result<()> {
        let assessed = self.inner.assess(assessments);
        let datasets: BTreeSet<&str> = assessments.iter().map(|a| a.dataset.as_str()).collect();
        for dataset in datasets {
            self.evict_dataset(dataset);
        }
        assessed
    }

    fn heads(&self) -> Result<BTreeMap<String, u64>> {
        self.inner.heads()
    }

    fn counts(&self) -> Result<BTreeMap<String, (u64, u64)>> {
        self.inner.counts()
    }

    fn lowest_kept(&self, dataset: &str) -> Result<Option<u64>> {
        self.inner.lowest_kept(dataset)
    }

    fn set_priority(
        &self,
        priority: &SourcePriority,
        against_updated_at_ns: i64,
    ) -> Result<std::result::Result<SourcePriority, Stale>> {
        self.inner.set_priority(priority, against_updated_at_ns)
    }

    fn priorities(&self) -> Result<Vec<SourcePriority>> {
        self.inner.priorities()
    }

    fn priority_changes(&self) -> Result<Vec<SourcePriority>> {
        self.inner.priority_changes()
    }

    fn record_want(&self, change: &WantChange) -> Result<()> {
        self.inner.record_want(change)
    }

    fn want_changes(&self) -> Result<Vec<WantChange>> {
        self.inner.want_changes()
    }

    fn served(&self) -> Result<Vec<Served>> {
        self.inner.served()
    }

    fn removals(&self) -> Result<Vec<Removal>> {
        self.inner.removals()
    }

    fn count_miss(&self, instance: &str, at_ns: i64) -> Result<()> {
        self.inner.count_miss(instance, at_ns)
    }

    fn miss_counts(&self) -> Result<BTreeMap<String, u64>> {
        self.inner.miss_counts()
    }

    fn keep_alias(&self, replaced: &str, stays: &str, at_ns: i64) -> Result<()> {
        self.inner.keep_alias(replaced, stays, at_ns)
    }

    fn aliases(&self) -> Result<BTreeMap<String, String>> {
        self.inner.aliases()
    }

    fn keep_configuration(&self, event: &EntitlementsChangedEvent) -> Result<()> {
        self.inner.keep_configuration(event)
    }

    fn dated_range(&self, dataset: &str) -> Result<Option<(String, String)>> {
        self.inner.dated_range(dataset)
    }

    fn dated_on(&self, dataset: &str, dates: &[String]) -> Result<Vec<Observation>> {
        self.inner.dated_on(dataset, dates)
    }

    fn closed_day_amendments(&self) -> Result<Vec<ClosedDayAmendment>> {
        self.inner.closed_day_amendments()
    }

    fn configuration(&self) -> Result<Option<EntitlementsChangedEvent>> {
        self.inner.configuration()
    }

    fn read_dataset(
        &self,
        dataset: &str,
        data_type: DataType,
        as_of_ns: i64,
    ) -> Result<Vec<Observation>> {
        self.inner.read_dataset(dataset, data_type, as_of_ns)
    }

    fn read_dataset_versions(
        &self,
        dataset: &str,
        data_type: DataType,
    ) -> Result<Vec<Observation>> {
        self.inner.read_dataset_versions(dataset, data_type)
    }

    fn row_version(&self, dataset: &str, row_key: &str, version: u64) -> Result<Vec<Observation>> {
        self.inner.row_version(dataset, row_key, version)
    }

    fn assessments(&self) -> Result<Vec<Assessment>> {
        self.inner.assessments()
    }

    fn adopt(&self) -> Result<u64> {
        self.inner.adopt()
    }

    fn epoch(&self) -> u64 {
        self.inner.epoch()
    }

    fn take_over(&self, by: &meridian_changes::TookOver) -> Result<u64> {
        self.inner.take_over(by)
    }

    fn head_epoch(&self) -> Result<u64> {
        self.inner.head_epoch()
    }

    fn read_changes(
        &self,
        partition: &str,
        after: u64,
        most_bytes: usize,
    ) -> std::result::Result<ReadChangesReply, ReadError> {
        self.inner.read_changes(partition, after, most_bytes)
    }
}
