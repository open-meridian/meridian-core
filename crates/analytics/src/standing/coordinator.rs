//! The coordinator's standing jobs (W12.1, W12.4; contract v30): registered
//! or joined by key, held by renewals, dirtied by what it hears, recomputed
//! as each measure's refresh says, and published to each holder.

use std::collections::BTreeSet;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use meridian_analytics_engine::compile;
use meridian_bus::{refusal_with_status, Bus, RefusalStatus, Stamp};
use meridian_domain::v1::{
    Envelope, Job, JobAnswer, ListDatasetsReply, ListDatasetsRequest, ReadJobAnswerRequest,
    RenewJobsReply, RenewJobsRequest, RunJobReply, RunJobRequest,
};
use prost::Message;

use super::{Asked, Held, Holder, Peers};
use crate::service::{client_of, job_id, Coordinator, OnBehalf};
use crate::{standing, topics};

/// How often a coordinator looks for cadence measures come due and holders
/// lapsed, when nothing wakes it sooner.
pub const TICK: Duration = Duration::from_millis(100);

/// What a computed answer's memory took of the budget.
enum Room {
    /// A first answer's, reserved whole.
    Held(meridian_budget::Reservation),
    /// A recomputation's growth, already in the job's reservation.
    #[allow(dead_code)]
    Grown(u64),
    /// A recomputation's shrinking, given back once taken.
    Shrink(u64),
    /// No budget is installed: a test, or a process with no limit.
    Unbounded,
}

/// How a standing job's first answer stood when its `RunJob` was answered.
enum Joined {
    First,
    Ready(Arc<JobAnswer>),
    Pending,
}

/// The status a refusal carries, where it is one: a refusal from another
/// coordinator stands as its answer; any other failure is the asking's.
fn status(refused: &str) -> Option<RefusalStatus> {
    meridian_bus::read_refusal_status(refused).map(|(status, _, _)| status)
}

fn not_held(job_id: &str) -> String {
    refusal_with_status(
        RefusalStatus::NotFound,
        &["job_id".to_string()],
        format!(
            "{job_id} is not held for this instance: a standing job no coordinator holds is \
             registered again by RunJob (spec/one-data-access-model section 4)"
        ),
    )
}

impl Coordinator {
    /// The live coordinators this one asks (question 13); one alone by
    /// default.
    pub fn use_peers(&self, peers: Arc<dyn Peers>) {
        *self.peers.write().expect("not poisoned") = peers;
    }

    fn peers_now(&self) -> Arc<dyn Peers> {
        Arc::clone(&self.peers.read().expect("not poisoned"))
    }

    /// The reads for `client` within `scope`.
    pub(crate) fn on_behalf(&self, client: &str, scope: &Option<Vec<String>>) -> OnBehalf {
        OnBehalf {
            bus: Arc::clone(&self.bus),
            handle: self.handle.clone(),
            stamp: Stamp {
                account_scope: scope.clone(),
                on_behalf_of_instance_id: client.to_string(),
                ..Stamp::default()
            },
            timeout: self.config.read_timeout,
        }
    }

    /// The datasets the client may read among those the job reads, as the
    /// lake's listing answers on its behalf: a standing job's key (question
    /// 11; narrowed to the job's own reads, 2026-11-10). A dataset whose
    /// declaration names no data type is counted, never left out.
    fn entitled(
        &self,
        asker: &OnBehalf,
        reads: &BTreeSet<&'static str>,
    ) -> Result<BTreeSet<String>, String> {
        use crate::resolve::Ask;
        let (kind, reply) = asker.ask(
            topics::LIST_DATASETS,
            "meridian.v1.ListDatasetsRequest",
            ListDatasetsRequest {}.encode_to_vec(),
        )?;
        if kind != "meridian.v1.ListDatasetsReply" {
            return Err(format!(
                "the lake's listing answered {kind}, not a ListDatasetsReply"
            ));
        }
        let listed = ListDatasetsReply::decode(&reply[..])
            .map_err(|failed| format!("the lake's listing did not decode: {failed}"))?;
        Ok(listed
            .datasets
            .into_iter()
            .filter(|listed| {
                let types = listed
                    .declaration
                    .as_ref()
                    .map(|declared| declared.data_types.clone())
                    .unwrap_or_default();
                types.is_empty() || types.iter().any(|t| reads.contains(t.as_str()))
            })
            .map(|listed| listed.dataset)
            .collect())
    }

    /// W12.1: a standing job registered, or joined where one with its key
    /// is held; handed to the coordinator its key falls to where that is
    /// another (question 13).
    pub(crate) fn run_standing(
        self: &Arc<Self>,
        job: Job,
        client: String,
        scope: Option<Vec<String>>,
        handed: bool,
    ) -> Result<RunJobReply, String> {
        let asker = self.on_behalf(&client, &scope);
        let compiled = compile::compile(&job).map_err(|refused| {
            refusal_with_status(
                RefusalStatus::InvalidArgument,
                &[format!("job.{}", refused.field)],
                refused.to_string(),
            )
        })?;
        let datasets = self.entitled(&asker, &standing::reads(&job, &compiled))?;
        let key = standing::key(&job, &scope, &datasets);
        if !handed {
            let peers = self.peers_now();
            let live = peers.live();
            let me = peers.me();
            if let Some(owner) = standing::owner(&key, &live).filter(|owner| **owner != me) {
                let asked = RunJobRequest {
                    job: Some(job.clone()),
                }
                .encode_to_vec();
                match peers.ask(owner, Asked::RunJob, asked, &client, &scope) {
                    Ok(reply) => {
                        return RunJobReply::decode(&reply[..]).map_err(|failed| {
                            format!("{owner}'s answer to a standing job did not decode: {failed}")
                        })
                    }
                    // A refusal is the job's, from its owner: it stands.
                    Err(refused) if status(&refused).is_some() => return Err(refused),
                    // Its owner gone between hearing and asking: held here,
                    // until the coordinators' set settles and a renewal
                    // moves it.
                    Err(failed) => {
                        tracing::warn!(%failed, owner, "a standing job's owner did not answer; held here");
                    }
                }
            }
        }
        let now = self.clock.now_ns();
        let fresh_holder = Holder {
            scope: scope.clone(),
            renewed_at_ns: now,
            sequence: 0,
            reading: None,
            told_later: false,
        };
        let (job_id, joined) = {
            let mut registry = self.standing.lock().expect("not poisoned");
            if let Some(job_id) = registry.by_key.get(&key).cloned() {
                let held = registry
                    .jobs
                    .get_mut(&job_id)
                    .expect("a key names a job held");
                let holder = held
                    .holders
                    .entry(client.clone())
                    .or_insert_with(|| fresh_holder.clone());
                holder.renewed_at_ns = now;
                holder.scope = scope.clone();
                let joined = match (&held.answer, &held.refused) {
                    (Some(answer), _) => Joined::Ready(Arc::clone(answer)),
                    (None, Some(refused)) => return Err(refused.clone()),
                    (None, None) => {
                        holder.told_later = true;
                        Joined::Pending
                    }
                };
                (job_id, joined)
            } else {
                if registry.jobs.len() >= self.config.standing.most {
                    return Err(refusal_with_status(
                        RefusalStatus::FailedPrecondition,
                        &["job.standing".to_string()],
                        format!(
                            "standing: this coordinator holds {} standing jobs, the most the \
                             deployment allows (analytics.standing.most): run it not standing, \
                             or later",
                            registry.jobs.len()
                        ),
                    ));
                }
                let job_id = job_id(now);
                let mut held = Held::new(job_id.clone(), key.clone(), job.clone());
                held.holders.insert(client.clone(), fresh_holder);
                registry.by_key.insert(key, job_id.clone());
                registry.jobs.insert(job_id.clone(), held);
                (job_id, Joined::First)
            }
        };
        tracing::info!(job_id, client, "a standing job held");
        let answered = match joined {
            Joined::Ready(answer) => Some(answer),
            Joined::Pending => None,
            Joined::First => {
                let (told, heard) = mpsc::channel();
                let me = Arc::clone(self);
                let id = job_id.clone();
                let every: Vec<usize> = (0..job.measures.len()).collect();
                std::thread::spawn(move || {
                    let computed = me.recompute(&id, every, true);
                    let _ = told.send(computed);
                });
                match heard.recv_timeout(self.config.inline) {
                    Ok(Err(refused)) => return Err(refused),
                    Ok(Ok(())) => {}
                    Err(_) => {}
                }
                let mut registry = self.standing.lock().expect("not poisoned");
                match registry.jobs.get_mut(&job_id) {
                    Some(held) => match &held.answer {
                        Some(answer) => Some(Arc::clone(answer)),
                        None => {
                            if let Some(holder) = held.holders.get_mut(&client) {
                                holder.told_later = true;
                            }
                            None
                        }
                    },
                    None => return Err(not_held(&job_id)),
                }
            }
        };
        match answered {
            Some(answer) if crate::pages::whole_fits(&answer, self.config.page_budget) => {
                Ok(RunJobReply {
                    answered_inline: true,
                    job_id,
                    answer: Some((*answer).clone()),
                })
            }
            // Past a page's budget, never inline: told, and read in pages.
            Some(answer) => {
                let hash = answer
                    .provenance
                    .as_ref()
                    .map(|p| p.answer_hash.clone())
                    .unwrap_or_default();
                self.tell(&job_id, &client, hash);
                Ok(RunJobReply {
                    answered_inline: false,
                    job_id,
                    answer: None,
                })
            }
            None => Ok(RunJobReply {
                answered_inline: false,
                job_id,
                answer: None,
            }),
        }
    }

    /// The measures `due` of a standing job recomputed: read again on its
    /// latest holder's behalf, as a job naming them alone, taken into its
    /// answer, and each change published to each holder. `first`: the
    /// job's first answer, every measure, which publishes nothing (it is
    /// the snapshot) and tells those answered before it was in.
    pub(crate) fn recompute(
        &self,
        job_id: &str,
        due: Vec<usize>,
        first: bool,
    ) -> Result<(), String> {
        let (sub, reader) = {
            let mut registry = self.standing.lock().expect("not poisoned");
            let Some(held) = registry.jobs.get_mut(job_id) else {
                return Ok(());
            };
            let Some(reader) = held.reader() else {
                return Ok(());
            };
            for &i in &due {
                held.clocks[i].dirty = false;
            }
            held.in_flight = true;
            (held.sub_job(&due), reader)
        };
        let asker = self.on_behalf(&reader.0, &reader.1);
        // Within the jobs running at once (analytics' jobs_at_once), as a
        // one-off job is: its inputs held only while it runs.
        let slot = self.standing_slot();
        let started = Instant::now();
        let computed = compile::compile(&sub)
            .map_err(|refused| {
                refusal_with_status(
                    RefusalStatus::InvalidArgument,
                    &[format!("job.{}", refused.field)],
                    refused.to_string(),
                )
            })
            .and_then(|compiled| self.answer_read(&asker, &sub, &compiled, job_id));
        drop(slot);
        let took = started.elapsed();
        // Its memory reserved against the component's budget before it is
        // held (stage M; approved 2026-11-10): a first answer that has no
        // room is refused for load with a retry-after; a recomputation that
        // would grow past it is left, its figures as they were, and asked
        // again on the next delivery.
        let computed = match computed {
            Ok((answer, inputs)) => match self.room_for(job_id, &due, &answer, first) {
                Ok(room) => Ok((answer, inputs, room)),
                Err(over) if first => Err(meridian_bus::overloaded(&over)),
                Err(over) => {
                    tracing::warn!(job_id, %over, "a standing job's figures have no room to grow; kept as they were");
                    let mut registry = self.standing.lock().expect("not poisoned");
                    if let Some(held) = registry.jobs.get_mut(job_id) {
                        held.in_flight = false;
                        for &i in &due {
                            held.clocks[i].dirty = true;
                        }
                    }
                    return Ok(());
                }
            },
            Err(refused) => Err(refused),
        };
        let now = self.clock.now_ns();
        let mut events = Vec::new();
        let mut told = Vec::new();
        let outcome = {
            let mut registry = self.standing.lock().expect("not poisoned");
            let Some(held) = registry.jobs.get_mut(job_id) else {
                // Stopped meanwhile: its last holder lapsed.
                return Ok(());
            };
            held.in_flight = false;
            match computed {
                Err(refused) if first => {
                    registry.stop(job_id);
                    self.standing_changed.notify_all();
                    Err(refused)
                }
                Err(failed) => {
                    // The answer stands as it was; the next delivery asks
                    // again.
                    tracing::warn!(job_id, %failed, "a standing job's figures could not be recomputed");
                    Ok(())
                }
                Ok((answer, inputs, room)) => {
                    standing::take(held, &due, &answer, &inputs, now);
                    match room {
                        Room::Held(reservation) => held.reservation = Some(reservation),
                        Room::Grown(_) | Room::Unbounded => {}
                        Room::Shrink(less) => {
                            if let Some(reservation) = &held.reservation {
                                reservation.shrink(less);
                            }
                        }
                    }
                    let spots = standing::spots(&inputs);
                    for &i in &due {
                        held.clocks[i].fitted_at_ns = now;
                        held.clocks[i].reference = spots.clone();
                    }
                    for (underlying, price) in &spots {
                        held.prices.entry(underlying.clone()).or_insert(*price);
                    }
                    let found = standing::leaves(&held.job, &inputs);
                    held.leaves.subjects.extend(found.subjects);
                    held.leaves.accounts.extend(found.accounts);
                    let whole =
                        standing::whole(held, answer.provenance.clone().unwrap_or_default());
                    if first {
                        let hash = whole
                            .provenance
                            .as_ref()
                            .map(|p| p.answer_hash.clone())
                            .unwrap_or_default();
                        for (client, holder) in held.holders.iter_mut() {
                            if std::mem::take(&mut holder.told_later) {
                                told.push((client.clone(), hash.clone()));
                            }
                        }
                    } else {
                        let changes = standing::changed(&whole, &held.published);
                        for (client, holder) in held.holders.iter_mut() {
                            events.extend(standing::events(
                                job_id,
                                client,
                                &mut holder.sequence,
                                &changes,
                            ));
                        }
                        tracing::info!(
                            job_id,
                            measures = due.len(),
                            changed = changes.len(),
                            holders = held.holders.len(),
                            took_ms = took.as_millis() as u64,
                            recomputed = held.recomputed,
                            "a standing job recomputed"
                        );
                    }
                    held.published = standing::published(&whole);
                    held.answer = Some(Arc::new(whole));
                    self.standing_changed.notify_all();
                    Ok(())
                }
            }
        };
        for event in events {
            if let Err(failed) = self.bus.publish(
                topics::DERIVED_SERIES_UPDATED,
                "meridian.v1.DerivedSeriesUpdatedEvent",
                event.encode_to_vec(),
                None,
                None,
            ) {
                tracing::warn!(%failed, job_id, "a standing job's figures could not be published");
            }
        }
        for (client, hash) in told {
            self.tell(job_id, &client, hash);
        }
        outcome
    }

    /// A budget of its own for the standing answers, in place of the
    /// process's: for a test.
    pub fn use_standing_budget(&self, budget: Arc<meridian_budget::Budget>) {
        *self.standing_budget.write().expect("not poisoned") = Some(budget);
    }

    /// The room a computed answer of `due` takes: a first answer's bytes
    /// reserved whole, waiting briefly; a recomputation's growth over what
    /// its measures held grown into the job's reservation, its shrinking
    /// given back after.
    fn room_for(
        &self,
        job_id: &str,
        due: &[usize],
        answer: &JobAnswer,
        first: bool,
    ) -> Result<Room, meridian_budget::Overloaded> {
        let bytes = standing::HELD_FACTOR * answer.encoded_len() as u64;
        let own = self.standing_budget.read().expect("not poisoned").clone();
        let budget: Option<&meridian_budget::Budget> = match &own {
            Some(own) => Some(own.as_ref()),
            None => meridian_budget::installed().map(|process| &process.budget),
        };
        let Some(budget) = budget else {
            return Ok(Room::Unbounded);
        };
        if first {
            return budget
                .reserve(
                    bytes,
                    &format!("standing job {job_id}"),
                    meridian_budget::WAIT,
                )
                .map(Room::Held);
        }
        let (handle, before) = {
            let registry = self.standing.lock().expect("not poisoned");
            let Some(held) = registry.jobs.get(job_id) else {
                return Ok(Room::Unbounded);
            };
            let before: u64 = due
                .iter()
                .map(|&i| standing::HELD_FACTOR * held.parts[i].bytes())
                .sum();
            match &held.reservation {
                Some(reservation) => ((**reservation).clone(), before),
                None => return Ok(Room::Unbounded),
            }
        };
        if bytes > before {
            handle.grow(bytes - before)?;
            Ok(Room::Grown(bytes - before))
        } else {
            Ok(Room::Shrink(before - bytes))
        }
    }

    /// What the coordinator heard: a lake row or a position change about
    /// what a standing job's inputs name dirties its measures (W12.4).
    pub fn standing_heard(&self, payload_type: &str, payload: &[u8]) {
        let mut registry = self.standing.lock().expect("not poisoned");
        if registry.jobs.is_empty() {
            return;
        }
        let Some(heard) = standing::heard(payload_type, payload) else {
            return;
        };
        let mut woke = false;
        for held in registry.jobs.values_mut() {
            let about = heard
                .subjects
                .iter()
                .any(|subject| held.leaves.subjects.contains(subject))
                || heard
                    .account
                    .as_ref()
                    .is_some_and(|account| held.leaves.accounts.contains(account));
            if !about {
                continue;
            }
            if let Some(price) = heard.price {
                for subject in &heard.subjects {
                    if held
                        .job
                        .chains
                        .iter()
                        .any(|chain| &chain.underlying_instrument_id == subject)
                    {
                        held.prices.insert(subject.clone(), price);
                    }
                }
            }
            for clock in held.clocks.iter_mut() {
                clock.dirty = true;
            }
            woke = true;
        }
        if woke {
            self.standing_changed.notify_all();
        }
    }

    /// Holders no renewal named within the lease let go, a job no holder
    /// holds stopped; then each job's measures due recomputed, one job
    /// after another.
    pub fn standing_tick(&self) {
        let now = self.clock.now_ns();
        let lease_ns = self.config.standing.lease.as_nanos() as i64;
        let due: Vec<(String, Vec<usize>)> = {
            let mut registry = self.standing.lock().expect("not poisoned");
            let mut stopped = Vec::new();
            for (job_id, held) in registry.jobs.iter_mut() {
                held.holders
                    .retain(|_, holder| now.saturating_sub(holder.renewed_at_ns) <= lease_ns);
                if held.holders.is_empty() {
                    stopped.push(job_id.clone());
                }
            }
            for job_id in &stopped {
                registry.stop(job_id);
                tracing::info!(
                    job_id,
                    "a standing job no renewal named within its lease stopped"
                );
            }
            registry
                .jobs
                .iter()
                .map(|(job_id, held)| (job_id.clone(), held.due(now)))
                .filter(|(_, due)| !due.is_empty())
                .collect()
        };
        for (job_id, measures) in due {
            if let Err(failed) = self.recompute(&job_id, measures, false) {
                tracing::warn!(job_id, %failed, "a standing job was not recomputed");
            }
        }
    }

    /// The standing jobs held now, and the measures each has recomputed:
    /// the engine's line per standing job.
    pub fn standing_held(&self) -> Vec<(String, usize, u64)> {
        self.standing
            .lock()
            .expect("not poisoned")
            .jobs
            .values()
            .map(|held| (held.job_id.clone(), held.holders.len(), held.recomputed))
            .collect()
    }

    /// W12.4: a sidecar's renewal of its plugin's standing jobs. Each held
    /// here for this client is renewed; one held here for others, or whose
    /// key no longer falls here, is answered unknown, the latter let go;
    /// the rest are asked of the other live coordinators, and those none
    /// holds are answered unknown.
    pub(crate) fn renew_jobs(
        &self,
        request: RenewJobsRequest,
        client: &str,
        handed: bool,
    ) -> Result<RenewJobsReply, String> {
        if !meridian_pb::bounds::RENEW_JOBS_REQUEST_JOB_IDS_COUNT.admits(request.job_ids.len()) {
            return Err(refusal_with_status(
                RefusalStatus::InvalidArgument,
                &["job_ids".to_string()],
                format!(
                    "job_ids names {}; a renewal names 1 to 256 standing jobs",
                    request.job_ids.len()
                ),
            ));
        }
        let peers = self.peers_now();
        let live = peers.live();
        let me = peers.me();
        let now = self.clock.now_ns();
        let mut unknown: BTreeSet<String> = BTreeSet::new();
        let mut elsewhere: BTreeSet<String> = BTreeSet::new();
        {
            let mut registry = self.standing.lock().expect("not poisoned");
            let mut moved = Vec::new();
            for job_id in &request.job_ids {
                match registry.jobs.get_mut(job_id) {
                    Some(held) if held.holders.contains_key(client) => {
                        let mine = standing::owner(&held.key, &live).is_none_or(|o| *o == me);
                        if mine {
                            if let Some(holder) = held.holders.get_mut(client) {
                                holder.renewed_at_ns = now;
                            }
                        } else {
                            moved.push(job_id.clone());
                            unknown.insert(job_id.clone());
                        }
                    }
                    // Held for other instances, never this one's to hold
                    // without registering it.
                    Some(_) => {
                        unknown.insert(job_id.clone());
                    }
                    None => {
                        elsewhere.insert(job_id.clone());
                    }
                }
            }
            for job_id in moved {
                registry.stop(&job_id);
                tracing::info!(
                    job_id,
                    "a standing job whose key falls to another coordinator let go"
                );
            }
        }
        if handed {
            unknown.extend(elsewhere);
        } else {
            for member in live.iter().filter(|member| **member != me) {
                if elsewhere.is_empty() {
                    break;
                }
                let asked = RenewJobsRequest {
                    job_ids: elsewhere.iter().cloned().collect(),
                };
                match peers.ask(
                    member,
                    Asked::RenewJobs,
                    asked.encode_to_vec(),
                    client,
                    &None,
                ) {
                    Ok(reply) => match RenewJobsReply::decode(&reply[..]) {
                        Ok(reply) => {
                            let theirs: BTreeSet<String> =
                                reply.unknown_job_ids.into_iter().collect();
                            elsewhere.retain(|job_id| theirs.contains(job_id));
                        }
                        Err(failed) => {
                            tracing::warn!(%failed, member, "a renewal's answer did not decode")
                        }
                    },
                    Err(failed) => {
                        tracing::warn!(%failed, member, "another coordinator did not answer a renewal")
                    }
                }
            }
            unknown.extend(elsewhere);
        }
        Ok(RenewJobsReply {
            unknown_job_ids: request
                .job_ids
                .iter()
                .filter(|job_id| unknown.contains(*job_id))
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
        })
    }

    /// W12.4: a standing job's answer, read by an instance holding it, one
    /// page at a time; a read from the first page takes the answer it pages
    /// through, so a recomputation between pages never mixes two. Waits for
    /// its first answer. None where the job is no standing job held here.
    pub(crate) fn read_standing(
        &self,
        request: &ReadJobAnswerRequest,
        client: &str,
    ) -> Option<Result<JobAnswer, String>> {
        let mut registry = self.standing.lock().expect("not poisoned");
        if !registry.jobs.contains_key(&request.job_id) {
            return None;
        }
        let until = Instant::now() + self.config.standing.first_answer_wait;
        loop {
            let Some(held) = registry.jobs.get_mut(&request.job_id) else {
                return Some(Err(not_held(&request.job_id)));
            };
            if !held.holders.contains_key(client) {
                return Some(Err(not_held(&request.job_id)));
            }
            if let Some(refused) = &held.refused {
                return Some(Err(refused.clone()));
            }
            if let Some(answer) = held.answer.clone() {
                let holder = held.holders.get_mut(client).expect("a holder");
                let reading = if request.cursor.is_empty() {
                    holder.reading = Some(Arc::clone(&answer));
                    answer
                } else {
                    holder.reading.clone().unwrap_or(answer)
                };
                drop(registry);
                return Some(crate::pages::page(
                    &reading,
                    request,
                    self.config.page_budget,
                ));
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Some(Err(refusal_with_status(
                    RefusalStatus::FailedPrecondition,
                    &["job_id".to_string()],
                    format!(
                        "{}'s first answer is not in yet: read it again shortly",
                        request.job_id
                    ),
                )));
            }
            registry = self
                .standing_changed
                .wait_timeout(registry, left)
                .expect("not poisoned")
                .0;
        }
    }

    /// A job's answer not held here, asked of each other live coordinator
    /// in turn (question 13): the first that holds it answers. None where
    /// none does.
    pub(crate) fn read_elsewhere(
        &self,
        request: &ReadJobAnswerRequest,
        client: &str,
        scope: &Option<Vec<String>>,
    ) -> Option<Result<JobAnswer, String>> {
        let peers = self.peers_now();
        let me = peers.me();
        for member in peers.live().iter().filter(|member| **member != me) {
            match peers.ask(
                member,
                Asked::ReadJobAnswer,
                request.encode_to_vec(),
                client,
                scope,
            ) {
                Ok(reply) => {
                    return Some(JobAnswer::decode(&reply[..]).map_err(|failed| {
                        format!(
                            "{member}'s page of {} did not decode: {failed}",
                            request.job_id
                        )
                    }))
                }
                Err(refused) if status(&refused) == Some(RefusalStatus::NotFound) => {}
                Err(refused) if status(&refused).is_some() => return Some(Err(refused)),
                Err(failed) => {
                    tracing::debug!(%failed, member, "another coordinator did not answer a read")
                }
            }
        }
        None
    }

    /// A question another coordinator asked on this one's own topic, for
    /// the client it names, answered here alone, never handed on.
    pub(crate) fn handed(
        self: &Arc<Self>,
        envelope: &Envelope,
    ) -> Result<(String, Vec<u8>), String> {
        let meta = envelope.meta.clone().unwrap_or_default();
        let client = meta.on_behalf_of_instance_id.clone();
        let scope = meta.account_scope_applies.then_some(meta.account_scope);
        if client.is_empty() {
            return Err("a handed question names its client".into());
        }
        let decoded = |what: &str| format!("an undecodable {what} was handed on");
        match standing::Asked::of(&envelope.payload_type) {
            Some(Asked::RunJob) => {
                let request = RunJobRequest::decode(&envelope.payload[..])
                    .map_err(|_| decoded("RunJobRequest"))?;
                self.run_job_for(request, client, scope, true)
                    .map(|reply| ("meridian.v1.RunJobReply".to_string(), reply.encode_to_vec()))
            }
            Some(Asked::RenewJobs) => {
                let request = RenewJobsRequest::decode(&envelope.payload[..])
                    .map_err(|_| decoded("RenewJobsRequest"))?;
                self.renew_jobs(request, &client, true).map(|reply| {
                    (
                        "meridian.v1.RenewJobsReply".to_string(),
                        reply.encode_to_vec(),
                    )
                })
            }
            Some(Asked::ReadJobAnswer) => {
                let request = ReadJobAnswerRequest::decode(&envelope.payload[..])
                    .map_err(|_| decoded("ReadJobAnswerRequest"))?;
                self.read_job_answer_for(request, &client, &scope, true)
                    .map(|reply| ("meridian.v1.JobAnswer".to_string(), reply.encode_to_vec()))
            }
            None => Err(format!(
                "{} is not a question one coordinator asks another",
                envelope.payload_type
            )),
        }
    }

    /// Until the process ends: woken by a delivery dirtying a job, else
    /// every [`TICK`], recomputing what is due and letting lapsed holders
    /// go.
    pub fn hold_standing_forever(self: &Arc<Self>) {
        let me = Arc::clone(self);
        std::thread::spawn(move || loop {
            {
                let registry = me.standing.lock().expect("not poisoned");
                let _ = me
                    .standing_changed
                    .wait_timeout(registry, TICK)
                    .expect("not poisoned");
            }
            me.standing_tick();
        });
    }
}

/// Serve a sidecar's renewals (`RenewJobs`).
pub fn serve_renewals(bus: &Arc<Bus>, coordinator: &Arc<Coordinator>) {
    let held = Arc::clone(coordinator);
    bus.serve(topics::RENEW_JOBS, move |envelope| {
        if envelope.payload_type != "meridian.v1.RenewJobsRequest" {
            return Err(format!(
                "expected meridian.v1.RenewJobsRequest, got {}",
                envelope.payload_type
            ));
        }
        let request = RenewJobsRequest::decode(&envelope.payload[..])
            .map_err(|failed| format!("undecodable meridian.v1.RenewJobsRequest: {failed}"))?;
        let (client, _) = client_of(&envelope);
        let reply = held.renew_jobs(request, &client, false)?;
        Ok((
            "meridian.v1.RenewJobsReply".to_string(),
            reply.encode_to_vec(),
        ))
    });
}

/// The coordinator's standing work on the bus: the questions other
/// coordinators hand it on its own topic, and what it hears for the jobs
/// it holds.
pub fn serve_standing(bus: &Arc<Bus>, coordinator: &Arc<Coordinator>) {
    let me = coordinator.peers_now().me();
    let held = Arc::clone(coordinator);
    bus.serve(&standing::asked_of(&me), move |envelope| {
        Ok(held
            .handed(&envelope)
            .unwrap_or_else(|refused| (standing::REFUSED_TYPE.to_string(), refused.into_bytes())))
    });
    for topic in standing::HEARD {
        let mut hearing = bus.subscribe_every(topic);
        let held = Arc::clone(coordinator);
        coordinator.handle.spawn(async move {
            while let Some(delivery) = hearing.recv().await {
                held.standing_heard(&delivery.envelope.payload_type, &delivery.envelope.payload);
            }
        });
    }
}
