//! The lake's store in memory, for tests: the same rules as the Postgres
//! store, one lock across each batch.

use std::collections::BTreeMap;
use std::sync::Mutex;

use meridian_domain::v1::{EntitlementsChangedEvent, SourcePriority};

use crate::row::Observation;
use crate::store::{matches, Query, Recorded, Removal, Result, Served, Stale, Store, WantChange};

#[derive(Default)]
struct Held {
    heads: BTreeMap<String, u64>,
    rows: Vec<Observation>,
    priorities: Vec<SourcePriority>,
    wants: Vec<WantChange>,
    served: Vec<Served>,
    removals: Vec<Removal>,
    misses: BTreeMap<String, u64>,
    aliases: BTreeMap<String, String>,
    configuration: Option<EntitlementsChangedEvent>,
}

#[derive(Default)]
pub struct MemoryStore {
    held: Mutex<Held>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Held> {
        self.held.lock().expect("lake store poisoned")
    }
}

/// Number a batch and decide each row's version against what is held
/// (and what the batch has numbered before it): the one rule both stores
/// apply, given the latest of a row key and the last sequence of a subject.
pub(crate) fn number(
    rows: Vec<Observation>,
    head: &mut u64,
    now_ns: i64,
    mut latest: impl FnMut(&Observation) -> Option<Observation>,
    mut previous: impl FnMut(&Observation) -> u64,
) -> Recorded {
    let mut done = Recorded::default();
    for mut row in rows {
        let earlier = done
            .rows
            .iter()
            .rev()
            .find(|held| {
                held.data_type() == row.data_type() && held.meta().row_key == row.meta().row_key
            })
            .cloned()
            .or_else(|| latest(&row));
        let version = match &earlier {
            Some(held) if held.same_value(&row) => {
                done.unchanged += 1;
                continue;
            }
            Some(held) => {
                done.restated += 1;
                held.meta().version + 1
            }
            None => {
                done.recorded += 1;
                1
            }
        };
        let previous_sequence = done
            .rows
            .iter()
            .rev()
            .find(|held| held.data_type() == row.data_type() && held.subject() == row.subject())
            .map(|held| held.meta().sequence)
            .unwrap_or_else(|| previous(&row));
        *head += 1;
        let meta = row.meta_mut();
        meta.version = version;
        meta.sequence = *head;
        meta.previous_sequence = previous_sequence;
        meta.recorded_at_ns = now_ns;
        done.rows.push(row);
    }
    done.head = *head;
    done
}

/// Each row key at its latest version recorded by `as_of_ns` (0 for now).
pub(crate) fn as_of(
    rows: impl IntoIterator<Item = Observation>,
    as_of_ns: i64,
) -> Vec<Observation> {
    let mut latest: BTreeMap<(String, i16, String), Observation> = BTreeMap::new();
    for row in rows {
        let meta = row.meta();
        if as_of_ns != 0 && meta.recorded_at_ns > as_of_ns {
            continue;
        }
        let key = (
            row.dataset().to_string(),
            row.data_type().code(),
            meta.row_key.clone(),
        );
        match latest.get(&key) {
            Some(held) if held.meta().version >= meta.version => {}
            _ => {
                latest.insert(key, row);
            }
        }
    }
    let mut out: Vec<Observation> = latest.into_values().collect();
    out.sort_by_key(|row| (row.dataset().to_string(), row.meta().sequence));
    out
}

impl Store for MemoryStore {
    fn record(&self, dataset: &str, rows: Vec<Observation>, now_ns: i64) -> Result<Recorded> {
        let mut held = self.lock();
        let mut head = held.heads.get(dataset).copied().unwrap_or(0);
        let stored = held.rows.clone();
        let done = number(
            rows,
            &mut head,
            now_ns,
            |row| {
                stored
                    .iter()
                    .filter(|h| {
                        h.dataset() == dataset
                            && h.data_type() == row.data_type()
                            && h.meta().row_key == row.meta().row_key
                    })
                    .max_by_key(|h| h.meta().version)
                    .cloned()
            },
            |row| {
                stored
                    .iter()
                    .filter(|h| {
                        h.dataset() == dataset
                            && h.data_type() == row.data_type()
                            && h.subject() == row.subject()
                    })
                    .map(|h| h.meta().sequence)
                    .max()
                    .unwrap_or(0)
            },
        );
        held.heads.insert(dataset.to_string(), head);
        held.rows.extend(done.rows.iter().cloned());
        Ok(done)
    }

    fn serve_unkept(
        &self,
        dataset: &str,
        rows: Vec<Observation>,
        readers: &[String],
        now_ns: i64,
    ) -> Result<Recorded> {
        let mut held = self.lock();
        let mut head = held.heads.get(dataset).copied().unwrap_or(0);
        let first = head + 1;
        let done = number(rows, &mut head, now_ns, |_| None, |_| 0);
        held.heads.insert(dataset.to_string(), head);
        if !done.rows.is_empty() {
            let served = served_of(dataset, &done.rows, first, head, readers, now_ns);
            held.served.push(served);
        }
        Ok(done)
    }

    fn read(&self, query: &Query) -> Result<Vec<Observation>> {
        let held = self.lock();
        let rows = held
            .rows
            .iter()
            .filter(|row| {
                query.datasets.iter().any(|d| d == row.dataset())
                    && row.subjects().iter().any(|s| query.subjects.contains(s))
                    && matches(row, query)
            })
            .cloned()
            .collect::<Vec<_>>();
        Ok(as_of(rows, query.as_of_ns))
    }

    fn heads(&self) -> Result<BTreeMap<String, u64>> {
        Ok(self.lock().heads.clone())
    }

    fn counts(&self) -> Result<BTreeMap<String, (u64, u64)>> {
        let held = self.lock();
        let mut counts: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        for row in &held.rows {
            let count = counts.entry(row.dataset().to_string()).or_default();
            count.0 += 1;
            if !row.meta().unconverted.is_empty() {
                count.1 += 1;
            }
        }
        Ok(counts)
    }

    fn set_priority(
        &self,
        priority: &SourcePriority,
        against_updated_at_ns: i64,
    ) -> Result<std::result::Result<SourcePriority, Stale>> {
        let mut held = self.lock();
        let standing = held
            .priorities
            .iter()
            .rev()
            .find(|p| p.data_type == priority.data_type && p.kind == priority.kind)
            .map_or(0, |p| p.updated_at_ns);
        if standing != against_updated_at_ns {
            return Ok(Err(Stale {
                standing_at_ns: standing,
            }));
        }
        held.priorities.push(priority.clone());
        Ok(Ok(priority.clone()))
    }

    fn priorities(&self) -> Result<Vec<SourcePriority>> {
        let held = self.lock();
        let mut latest: BTreeMap<(String, i32), SourcePriority> = BTreeMap::new();
        for p in &held.priorities {
            latest.insert((p.data_type.clone(), p.kind), p.clone());
        }
        Ok(latest.into_values().collect())
    }

    fn priority_changes(&self) -> Result<Vec<SourcePriority>> {
        Ok(self.lock().priorities.clone())
    }

    fn record_want(&self, change: &WantChange) -> Result<()> {
        self.lock().wants.push(change.clone());
        Ok(())
    }

    fn want_changes(&self) -> Result<Vec<WantChange>> {
        Ok(self.lock().wants.clone())
    }

    fn served(&self) -> Result<Vec<Served>> {
        Ok(self.lock().served.clone())
    }

    fn remove_before(
        &self,
        dataset: &str,
        recorded_before_ns: i64,
        why: &str,
        now_ns: i64,
    ) -> Result<u64> {
        let mut held = self.lock();
        let before = held.rows.len();
        held.rows.retain(|row| {
            row.dataset() != dataset || row.meta().recorded_at_ns >= recorded_before_ns
        });
        let removed = (before - held.rows.len()) as u64;
        if removed > 0 {
            held.removals.push(Removal {
                dataset: dataset.to_string(),
                rows: removed,
                recorded_before_ns,
                why: why.to_string(),
                at_ns: now_ns,
            });
        }
        Ok(removed)
    }

    fn removals(&self) -> Result<Vec<Removal>> {
        Ok(self.lock().removals.clone())
    }

    fn count_miss(&self, instance: &str, _at_ns: i64) -> Result<()> {
        *self.lock().misses.entry(instance.to_string()).or_default() += 1;
        Ok(())
    }

    fn miss_counts(&self) -> Result<BTreeMap<String, u64>> {
        Ok(self.lock().misses.clone())
    }

    fn keep_alias(&self, replaced: &str, stays: &str, _at_ns: i64) -> Result<()> {
        self.lock()
            .aliases
            .insert(replaced.to_string(), stays.to_string());
        Ok(())
    }

    fn aliases(&self) -> Result<BTreeMap<String, String>> {
        Ok(self.lock().aliases.clone())
    }

    fn keep_configuration(&self, event: &EntitlementsChangedEvent) -> Result<()> {
        self.lock().configuration = Some(event.clone());
        Ok(())
    }

    fn configuration(&self) -> Result<Option<EntitlementsChangedEvent>> {
        Ok(self.lock().configuration.clone())
    }
}

/// What a served batch leaves: its subjects and the fields it carried.
pub(crate) fn served_of(
    dataset: &str,
    rows: &[Observation],
    first: u64,
    last: u64,
    readers: &[String],
    now_ns: i64,
) -> Served {
    let mut subjects: Vec<String> = rows.iter().flat_map(|r| r.subjects()).collect();
    subjects.sort();
    subjects.dedup();
    let mut fields: Vec<String> = rows
        .iter()
        .flat_map(|r| {
            meridian_domain::lake::fields_of(r.data_type().name())
                .iter()
                .map(|f| f.to_string())
        })
        .collect();
    fields.sort();
    fields.dedup();
    Served {
        dataset: dataset.to_string(),
        first_sequence: first,
        last_sequence: last,
        subjects,
        fields,
        readers: readers.to_vec(),
        at_ns: now_ns,
    }
}
