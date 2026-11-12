//! The lake's store in memory, for tests: the same rules as the Postgres
//! store, one lock across each batch.

use std::collections::BTreeMap;
use std::sync::Mutex;

use meridian_domain::v1::{EntitlementsChangedEvent, SourcePriority};

use crate::row::{DataType, Observation};
use crate::store::{
    as_assessed, as_flagged, closed_reason, counted, force_order, matches, Assessment,
    ClosedDayAmendment, Query, Recorded, Removal, Result, Served, Stale, Store, StoreError,
    WantChange, WriteAheadBatch, WriteAheadPosition,
};

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
    amendments: Vec<ClosedDayAmendment>,
    assessments: Vec<Assessment>,
    /// The `control` partition's epoch, every dataset's with it (step 3a).
    epoch: u64,
    /// Each dataset's lowest sequence its log still reaches.
    kept_from: BTreeMap<String, u64>,
    /// The epoch each row and range served was written under.
    epochs: BTreeMap<(String, u64), u64>,
    /// The `control` partition's log (step 3b): assessments, amendments,
    /// priorities and aliases, in the order committed.
    control: Vec<meridian_changes::Change>,
    /// The write-ahead stream's position, by stream (the `ha` profile).
    write_ahead: BTreeMap<String, WriteAheadPosition>,
    /// What each stream dropped and each dataset's latest valid time from
    /// it (W10.14).
    dropped: BTreeMap<String, Vec<crate::store::Dropped>>,
    valid: BTreeMap<(String, String), i64>,
    /// Every dataset gap's every state, in order (W10.14).
    gaps: Vec<meridian_domain::v1::DatasetGap>,
    /// Every switch of a default read's source, in order (W10.6).
    switches: Vec<meridian_domain::v1::SourceSwitched>,
}

#[derive(Default)]
pub struct MemoryStore {
    held: Mutex<Held>,
    fence: meridian_changes::Fence,
}

impl MemoryStore {
    /// Hold this store's writer at `epoch`, as a deposed writer holds the
    /// one it had: what a test of the fence does.
    #[cfg(test)]
    pub(crate) fn fence_for_test(&self, epoch: u64) {
        self.fence.adopt(epoch);
    }

    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Held> {
        self.held.lock().expect("lake store poisoned")
    }

    /// Log what is not a dataset's row in the `control` partition (step
    /// 3b), under the writer's epoch as Postgres checks it.
    fn log_control(&self, held: &mut Held, entries: Vec<(&str, Vec<u8>, i64)>) -> Result<()> {
        let epoch = self.fenced(held)?;
        for (kind, body, at_ns) in entries {
            let sequence = held.control.len() as u64 + 1;
            held.control.push(crate::store::control_change(
                sequence, epoch, at_ns, kind, body,
            ));
        }
        Ok(())
    }

    /// The batch's epoch, checked against the partitions' as Postgres
    /// checks it under the head's lock.
    fn fenced(&self, held: &mut Held) -> Result<u64> {
        if held.epoch == 0 {
            held.epoch = 1;
        }
        let epoch = self.fence.adopt_once(held.epoch);
        self.fence.admits(held.epoch)?;
        Ok(epoch)
    }
}

/// Number a batch and decide each row's version against what is held
/// (and what the batch has numbered before it): the one rule both stores
/// apply, given the latest of a row key and the last sequence of a subject.
pub(crate) fn number(
    rows: Vec<Observation>,
    head: &mut u64,
    now_ns: i64,
    latest: impl FnMut(&Observation) -> Option<Observation>,
    previous: impl FnMut(&Observation) -> u64,
) -> Recorded {
    number_parts(vec![rows], head, now_ns, latest, previous)
        .pop()
        .unwrap_or_default()
}

/// [`number`], for several batches of one dataset numbered in turn as one
/// (the write-ahead stream's, the resilience spec's section 4): each part is
/// decided against what is held and what every part before it numbered, and
/// answered with what it did itself.
pub(crate) fn number_parts(
    parts: Vec<Vec<Observation>>,
    head: &mut u64,
    now_ns: i64,
    mut latest: impl FnMut(&Observation) -> Option<Observation>,
    mut previous: impl FnMut(&Observation) -> u64,
) -> Vec<Recorded> {
    // What the parts have numbered so far: each row key's last version, and
    // each subject's last sequence, of the rows numbered.
    let mut keys: std::collections::HashMap<(i16, String), Observation> =
        std::collections::HashMap::new();
    let mut subjects: std::collections::HashMap<(i16, String), u64> =
        std::collections::HashMap::new();
    let mut answered = Vec::with_capacity(parts.len());
    for rows in parts {
        let mut done = Recorded::default();
        for mut row in rows {
            let key = (row.data_type().code(), row.meta().row_key.clone());
            let earlier = keys.get(&key).cloned().or_else(|| latest(&row));
            let version = match &earlier {
                Some(held) if held.same_value(&row) => {
                    done.unchanged += 1;
                    continue;
                }
                // A rate's confirmation (contract v27, W10.4): only its raw
                // reference changed. Recorded, as the next version keeping the
                // reference, and counted unchanged, never a restatement.
                Some(held) if held.confirms(&row) => {
                    done.unchanged += 1;
                    held.meta().version + 1
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
            let subject = (row.data_type().code(), row.subject().to_string());
            let previous_sequence = subjects
                .get(&subject)
                .copied()
                .unwrap_or_else(|| previous(&row));
            *head += 1;
            let meta = row.meta_mut();
            meta.version = version;
            meta.sequence = *head;
            meta.previous_sequence = previous_sequence;
            meta.recorded_at_ns = now_ns;
            keys.insert(key, row.clone());
            subjects.insert(subject, *head);
            done.rows.push(row);
        }
        done.head = *head;
        answered.push(done);
    }
    answered
}

/// Each row key at its version in force among those recorded by `as_of_ns`
/// (0 for now): its latest version, a rate's by published time (ruling
/// 1006 (b); [`crate::store::force_order`]).
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
            Some(held) if force_order(held) >= force_order(&row) => {}
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
        let epoch = self.fenced(&mut held)?;
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
        for row in &done.rows {
            held.epochs
                .insert((dataset.to_string(), row.meta().sequence), epoch);
        }
        held.rows.extend(done.rows.iter().cloned());
        Ok(done)
    }

    fn write_ahead_position(&self, stream: &str) -> Result<Option<WriteAheadPosition>> {
        Ok(self.lock().write_ahead.get(stream).copied())
    }

    fn record_write_ahead(&self, batch: WriteAheadBatch, now_ns: i64) -> Result<Vec<Recorded>> {
        let mut held = self.lock();
        let epoch = self.fenced(&mut held)?;
        let at = held.write_ahead.get(&batch.stream).copied();
        if at != batch.after {
            return Err(StoreError::PositionMoved(format!(
                "the lake's place in {} is {at:?}, not {:?}",
                batch.stream, batch.after
            )));
        }
        let mut answered = vec![Recorded::default(); batch.parts.len()];
        let mut by_dataset: BTreeMap<String, Vec<(usize, Vec<Observation>)>> = BTreeMap::new();
        for (at, (dataset, rows)) in batch.parts.into_iter().enumerate() {
            by_dataset.entry(dataset).or_default().push((at, rows));
        }
        for (dataset, parts) in by_dataset {
            let mut head = held.heads.get(&dataset).copied().unwrap_or(0);
            let stored = &held.rows;
            let (places, rows): (Vec<usize>, Vec<Vec<Observation>>) = parts.into_iter().unzip();
            let done = number_parts(
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
            held.heads.insert(dataset.clone(), head);
            for (place, done) in places.into_iter().zip(done) {
                for row in &done.rows {
                    held.epochs
                        .insert((dataset.clone(), row.meta().sequence), epoch);
                }
                held.rows.extend(done.rows.iter().cloned());
                answered[place] = done;
            }
        }
        held.dropped
            .entry(batch.stream.clone())
            .or_default()
            .extend(batch.dropped);
        for (dataset, valid_ns) in batch.valid {
            let at = held
                .valid
                .entry((batch.stream.clone(), dataset))
                .or_insert(valid_ns);
            *at = (*at).max(valid_ns);
        }
        let logged: Vec<(&str, Vec<u8>, i64)> = batch
            .gaps
            .iter()
            .map(|gap| {
                use prost::Message as _;
                (
                    crate::store::CONTROL_GAP,
                    gap.encode_to_vec(),
                    gap.recorded_at_ns,
                )
            })
            .collect();
        self.log_control(&mut held, logged)?;
        held.gaps.extend(batch.gaps);
        held.write_ahead.insert(batch.stream, batch.through);
        Ok(answered)
    }

    fn write_ahead_found(
        &self,
        stream: &str,
    ) -> Result<(Vec<crate::store::Dropped>, BTreeMap<String, i64>)> {
        let held = self.lock();
        Ok((
            held.dropped.get(stream).cloned().unwrap_or_default(),
            held.valid
                .iter()
                .filter(|((at, _), _)| at == stream)
                .map(|((_, dataset), valid)| (dataset.clone(), *valid))
                .collect(),
        ))
    }

    fn record_gap(&self, gap: &meridian_domain::v1::DatasetGap) -> Result<()> {
        use prost::Message as _;
        let mut held = self.lock();
        self.log_control(
            &mut held,
            vec![(
                crate::store::CONTROL_GAP,
                gap.encode_to_vec(),
                gap.recorded_at_ns,
            )],
        )?;
        held.gaps.push(gap.clone());
        Ok(())
    }

    fn dataset_gaps(&self) -> Result<Vec<meridian_domain::v1::DatasetGap>> {
        Ok(self.lock().gaps.clone())
    }

    fn record_switches(&self, switches: &[meridian_domain::v1::SourceSwitched]) -> Result<()> {
        use prost::Message as _;
        let mut held = self.lock();
        let logged = switches
            .iter()
            .map(|switch| {
                (
                    crate::store::CONTROL_SWITCH,
                    switch.encode_to_vec(),
                    switch.switched_at_ns,
                )
            })
            .collect();
        self.log_control(&mut held, logged)?;
        held.switches.extend(switches.iter().cloned());
        Ok(())
    }

    fn source_switches(&self) -> Result<Vec<meridian_domain::v1::SourceSwitched>> {
        Ok(self.lock().switches.clone())
    }

    fn serve_unkept(
        &self,
        dataset: &str,
        rows: Vec<Observation>,
        readers: &[String],
        now_ns: i64,
    ) -> Result<Recorded> {
        let mut held = self.lock();
        let epoch = self.fenced(&mut held)?;
        let mut head = held.heads.get(dataset).copied().unwrap_or(0);
        let first = head + 1;
        let done = number(rows, &mut head, now_ns, |_| None, |_| 0);
        held.heads.insert(dataset.to_string(), head);
        if !done.rows.is_empty() {
            held.epochs.insert((dataset.to_string(), head), epoch);
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
        let mut out = if crate::store::every_version(query) {
            crate::store::recorded_by(rows, query.as_of_ns)
        } else {
            as_of(rows, query.as_of_ns)
        };
        if query.published_as_of_ns != 0 {
            out = crate::read::as_published(out, query.published_as_of_ns);
        }
        as_flagged(&mut out, &held.amendments, query.as_of_ns);
        as_assessed(&mut out, &held.assessments, query.as_of_ns);
        Ok(out)
    }

    fn rows_at(
        &self,
        data_type: DataType,
        places: &[(String, u64)],
        as_of_ns: i64,
    ) -> Result<Vec<Observation>> {
        let held = self.lock();
        let by_place: BTreeMap<(&str, u64), &Observation> = held
            .rows
            .iter()
            .filter(|row| row.data_type() == data_type)
            .map(|row| ((row.dataset(), row.meta().sequence), row))
            .collect();
        let mut out: Vec<Observation> = places
            .iter()
            .filter_map(|(dataset, sequence)| by_place.get(&(dataset.as_str(), *sequence)))
            .map(|row| (*row).clone())
            .collect();
        as_flagged(&mut out, &held.amendments, as_of_ns);
        as_assessed(&mut out, &held.assessments, as_of_ns);
        Ok(out)
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

    fn lowest_kept(&self, dataset: &str) -> Result<Option<u64>> {
        Ok(self
            .lock()
            .rows
            .iter()
            .filter(|row| row.dataset() == dataset)
            .map(|row| row.meta().sequence)
            .min())
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
        self.log_control(
            &mut held,
            vec![(
                crate::store::CONTROL_PRIORITY,
                prost::Message::encode_to_vec(priority),
                priority.updated_at_ns,
            )],
        )?;
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
        self.fenced(&mut held)?;
        let last_gone = held
            .rows
            .iter()
            .filter(|row| {
                row.dataset() == dataset && row.meta().recorded_at_ns < recorded_before_ns
            })
            .map(|row| row.meta().sequence)
            .max();
        let before = held.rows.len();
        held.rows.retain(|row| {
            row.dataset() != dataset || row.meta().recorded_at_ns >= recorded_before_ns
        });
        let removed = (before - held.rows.len()) as u64;
        if let Some(last) = last_gone {
            let kept = held.kept_from.entry(dataset.to_string()).or_insert(1);
            *kept = (*kept).max(last + 1);
        }
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

    fn keep_alias(&self, replaced: &str, stays: &str, at_ns: i64) -> Result<()> {
        let mut held = self.lock();
        if held.aliases.contains_key(replaced) {
            return Ok(());
        }
        self.log_control(
            &mut held,
            vec![(
                crate::store::CONTROL_ALIAS,
                crate::store::alias_body(replaced, stays, at_ns),
                at_ns,
            )],
        )?;
        held.aliases.insert(replaced.to_string(), stays.to_string());
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

    fn dated_range(&self, dataset: &str) -> Result<Option<(String, String)>> {
        let held = self.lock();
        let dates: Vec<&str> = held
            .rows
            .iter()
            .filter(|row| row.dataset() == dataset && !row.meta().business_date.is_empty())
            .map(|row| row.meta().business_date.as_str())
            .collect();
        Ok(match (dates.iter().min(), dates.iter().max()) {
            (Some(first), Some(last)) => Some((first.to_string(), last.to_string())),
            _ => None,
        })
    }

    fn read_dataset(
        &self,
        dataset: &str,
        data_type: crate::row::DataType,
        as_of_ns: i64,
        published_as_of_ns: i64,
    ) -> Result<Vec<Observation>> {
        let held = self.lock();
        let rows = held
            .rows
            .iter()
            .filter(|row| row.dataset() == dataset && row.data_type() == data_type)
            .cloned()
            .collect::<Vec<_>>();
        let mut out = if published_as_of_ns != 0 {
            crate::read::as_published(
                crate::store::recorded_by(rows, as_of_ns),
                published_as_of_ns,
            )
        } else {
            as_of(rows, as_of_ns)
        };
        as_assessed(&mut out, &held.assessments, as_of_ns);
        Ok(out)
    }

    fn read_dataset_versions(
        &self,
        dataset: &str,
        data_type: crate::row::DataType,
    ) -> Result<Vec<Observation>> {
        let held = self.lock();
        let mut rows: Vec<Observation> = held
            .rows
            .iter()
            .filter(|row| row.dataset() == dataset && row.data_type() == data_type)
            .cloned()
            .collect();
        rows.sort_by_key(|row| row.meta().sequence);
        as_assessed(&mut rows, &held.assessments, 0);
        Ok(rows)
    }

    fn dated_on(
        &self,
        dataset: &str,
        dates: &[String],
        after: u64,
        most: usize,
    ) -> Result<Vec<Observation>> {
        let held = self.lock();
        let mut rows: Vec<Observation> = held
            .rows
            .iter()
            .filter(|row| {
                row.dataset() == dataset
                    && row.meta().sequence > after
                    && dates.contains(&row.meta().business_date)
            })
            .cloned()
            .collect();
        rows.sort_by_key(|row| row.meta().sequence);
        rows.truncate(most);
        as_flagged(&mut rows, &held.amendments, 0);
        Ok(rows)
    }

    fn amend_closed_day(&self, amendments: &[ClosedDayAmendment]) -> Result<()> {
        let mut held = self.lock();
        let logged = amendments
            .iter()
            .map(|a| {
                (
                    crate::store::CONTROL_CLOSED_DAY,
                    crate::store::amendment_body(a),
                    a.at_ns,
                )
            })
            .collect();
        self.log_control(&mut held, logged)?;
        held.amendments.extend(amendments.iter().cloned());
        Ok(())
    }

    fn closed_day_amendments(&self) -> Result<Vec<ClosedDayAmendment>> {
        Ok(self.lock().amendments.clone())
    }

    fn quality_counts(&self) -> Result<BTreeMap<String, (u64, u64, u64)>> {
        use meridian_domain::v1::QualityState;
        let held = self.lock();
        // The rows in force: each row key at its version in force, as a
        // default read takes it (ruling 1006 (b)).
        let mut rows = as_of(held.rows.iter().cloned(), 0);
        as_assessed(&mut rows, &held.assessments, 0);
        let mut counts: BTreeMap<String, (u64, u64, u64)> = BTreeMap::new();
        for row in &rows {
            let count = counts.entry(row.dataset().to_string()).or_default();
            match crate::store::rank(row.meta().quality) {
                state if state == QualityState::Invalid as i32 => count.2 += 1,
                state if state == QualityState::Suspicious as i32 => count.1 += 1,
                _ => count.0 += 1,
            }
        }
        Ok(counts)
    }

    fn unconverted_counts(&self) -> Result<BTreeMap<String, u64>> {
        // The rows in force (ruling 1006 (c)).
        let held = self.lock();
        let mut counts: BTreeMap<String, u64> = BTreeMap::new();
        for row in as_of(held.rows.iter().cloned(), 0) {
            if !row.meta().unconverted.is_empty() {
                *counts.entry(row.dataset().to_string()).or_default() += 1;
            }
        }
        Ok(counts)
    }

    fn closed_day_counts(&self) -> Result<BTreeMap<String, (u64, u64)>> {
        // The rows in force (ruling 1006 (c)), by the flag as known now.
        let held = self.lock();
        let mut rows = as_of(held.rows.iter().cloned(), 0);
        as_flagged(&mut rows, &held.amendments, 0);
        let mut counts: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        for row in &rows {
            let (closed, outside) = counted(closed_reason(row.meta().closed_day.as_ref()));
            let count = counts.entry(row.dataset().to_string()).or_default();
            count.0 += closed;
            count.1 += outside;
        }
        Ok(counts)
    }

    fn row_version(&self, dataset: &str, row_key: &str, version: u64) -> Result<Vec<Observation>> {
        let held = self.lock();
        let mut rows: Vec<Observation> = held
            .rows
            .iter()
            .filter(|row| {
                row.dataset() == dataset
                    && row.meta().row_key == row_key
                    && row.meta().version == version
            })
            .cloned()
            .collect();
        as_assessed(&mut rows, &held.assessments, 0);
        Ok(rows)
    }

    fn assess(&self, assessments: &[Assessment]) -> Result<()> {
        let mut held = self.lock();
        let logged = assessments
            .iter()
            .map(|a| {
                (
                    crate::store::CONTROL_ASSESSMENT,
                    crate::store::assessment_body(a),
                    a.at_ns,
                )
            })
            .collect();
        self.log_control(&mut held, logged)?;
        held.assessments.extend(assessments.iter().cloned());
        Ok(())
    }

    fn assessments(&self) -> Result<Vec<Assessment>> {
        Ok(self.lock().assessments.clone())
    }

    fn adopt(&self) -> Result<u64> {
        let mut held = self.lock();
        if held.epoch == 0 {
            held.epoch = 1;
        }
        Ok(self.fence.adopt_once(held.epoch))
    }

    fn epoch(&self) -> u64 {
        self.fence.epoch()
    }

    fn take_over(&self, _by: &meridian_changes::TookOver) -> Result<u64> {
        let mut held = self.lock();
        held.epoch = held.epoch.max(1) + 1;
        self.fence.adopt(held.epoch);
        Ok(held.epoch)
    }

    fn head_epoch(&self) -> Result<u64> {
        Ok(self.lock().epoch.max(1))
    }

    fn read_changes(
        &self,
        partition: &str,
        after: u64,
        most_bytes: usize,
    ) -> std::result::Result<meridian_changes::ReadChangesReply, meridian_changes::ReadError> {
        use meridian_changes::{ReadChangesReply, ReadError};
        let held = self.lock();
        let epoch = held.epoch.max(1);
        if partition == meridian_changes::CONTROL {
            let head = held.control.len() as u64;
            let lowest_kept = u64::from(head > 0);
            meridian_changes::check_kept(after, lowest_kept, head)?;
            let mut page = meridian_changes::Page::new(most_bytes);
            for change in held.control.iter().filter(|c| c.sequence > after) {
                if !page.take(change.clone()) {
                    break;
                }
            }
            return Ok(ReadChangesReply {
                changes: page.changes,
                head,
                lowest_kept,
                epoch,
                after_sequence: 0,
            });
        }
        let head = *held
            .heads
            .get(partition)
            .ok_or_else(|| ReadError::UnknownPartition(partition.to_string()))?;
        let kept_from = held.kept_from.get(partition).copied().unwrap_or(1);
        let lowest_kept = if head >= kept_from { kept_from } else { 0 };
        meridian_changes::check_kept(after, lowest_kept, head)?;
        let origin = meridian_changes::origin();
        let written = |sequence: u64| {
            held.epochs
                .get(&(partition.to_string(), sequence))
                .copied()
                .unwrap_or(epoch)
        };
        let from = after.max(kept_from.saturating_sub(1));
        let mut changes: Vec<meridian_changes::Change> = held
            .rows
            .iter()
            .filter(|row| row.dataset() == partition && row.meta().sequence > from)
            .map(|row| {
                crate::store::row_change(partition, row, written(row.meta().sequence), origin)
            })
            .chain(
                held.served
                    .iter()
                    .filter(|served| served.dataset == partition && served.last_sequence > from)
                    .map(|served| {
                        crate::store::served_change(served, written(served.last_sequence), origin)
                    }),
            )
            .collect();
        changes.sort_by_key(|change| change.sequence);
        let mut page = meridian_changes::Page::new(most_bytes);
        for change in changes {
            if !page.take(change) {
                break;
            }
        }
        Ok(ReadChangesReply {
            changes: page.changes,
            head,
            lowest_kept,
            epoch,
            after_sequence: 0,
        })
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
