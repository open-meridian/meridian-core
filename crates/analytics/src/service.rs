//! The coordinator (W12.1 to W12.5): compiles a job, resolves its inputs on
//! the client's behalf, prices each routed model id's batch through its
//! `pricing` plugin, splits the answer into units for the worker pool, and
//! answers inline -- or, past the request's window, later, telling the
//! client by `JobFinished` and answering `ReadJobAnswer` (Q5).
//!
//! Who asks is the client instance its sidecar stamped as the publisher,
//! within the read scope it stamped on the job (W12.2): an account outside
//! it is refused naming it, and every store read carries both.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{mpsc, Arc, Condvar, Mutex, RwLock};
use std::time::Duration;

use meridian_analytics_engine::canonical;
use meridian_analytics_engine::case::Case;
use meridian_analytics_engine::compile;
use meridian_analytics_engine::definitions;
use meridian_analytics_engine::graph::{self, Executor};
use meridian_analytics_engine::pricing;
use meridian_bus::{refusal_with_status, Bus, RefusalStatus, Stamp};
use meridian_clock::Clock;
use meridian_domain::exact::Exact;
use meridian_domain::v1::{
    AnswerPriceBatchReply, Envelope, JobAnswer, JobFinishedEvent, ListFinishedJobsReply,
    ListFinishedJobsRequest, ListMeasureDefinitionsReply, ListMeasureDefinitionsRequest,
    ListPriceBatchesReply, ListPriceBatchesRequest, MarketObjectValue, Money, PriceBatchReply,
    PriceBatchRequest, PricingUnit, ReadJobAnswerRequest, RunJobReply, RunJobRequest, ScenarioKey,
    ScenarioKind,
};
use prost::Message;
use serde_json::{json, Value};

use crate::resolve::{self, Ask};
use crate::standing::{self, Peers};
use crate::{topics, wire};

/// How the coordinator runs.
#[derive(Debug, Clone)]
pub struct Config {
    /// The version every answer names.
    pub engine_version: String,
    /// How long a job may take and still be answered in the reply (Q5).
    pub inline: Duration,
    /// How long a routed plugin has to answer its batch (W12.3).
    pub price_deadline: Duration,
    /// How long a store read may take.
    pub read_timeout: Duration,
    /// How many finished answers are kept to be read (Q12: losable).
    pub answers_kept: usize,
    /// The most bytes the answers kept hold in all, as encoded, where they
    /// are kept (found 2026-11-09: kept by count alone, a thousand chain
    /// answers of about 1.2 MB each could hold about 1.2 GB of a 2 GiB
    /// coordinator). The oldest leaves first; an answer larger than all of
    /// it is kept as a refusal saying so, never pushing every other out.
    pub answers_kept_bytes: usize,
    /// How long a finished answer is kept to be read; past it, it has left
    /// the cache and the job is run again (Q12).
    pub answers_kept_for: Duration,
    /// How many jobs resolve and compute at once, each holding its inputs
    /// while it runs (found 2026-11-09: a thread a job and no bound, so a
    /// burst of chain jobs held every chain's inputs at once). A job past
    /// it waits for one to finish, answered later as any job past its
    /// window is.
    pub jobs_at_once: usize,
    /// How many jobs may wait for one running to finish; one past it is
    /// refused, saying so, and asked again later.
    pub jobs_waiting: usize,
    /// Where a page of an answer ends, in bytes (ruling 794): an answer past
    /// it is never given inline, and is read in pages of it.
    pub page_budget: usize,
    /// Contract v30 (W12.4): how standing jobs are held.
    pub standing: standing::Settings,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            engine_version: crate::engine_version(""),
            inline: Duration::from_secs(2),
            price_deadline: Duration::from_secs(5),
            read_timeout: Duration::from_secs(8),
            answers_kept: 1_000,
            answers_kept_bytes: ANSWERS_KEPT_BYTES,
            answers_kept_for: ANSWERS_KEPT_FOR,
            jobs_at_once: JOBS_AT_ONCE,
            jobs_waiting: JOBS_WAITING,
            page_budget: crate::pages::PAGE_BUDGET,
            standing: standing::Settings::default(),
        }
    }
}

/// The answers kept, in bytes as encoded: a quarter of the coordinator's
/// 2 GiB (the chart's limit), about two hundred chain answers of dev's
/// 1.2 MB.
pub const ANSWERS_KEPT_BYTES: usize = 256 * 1024 * 1024;

/// How long an answer is kept to be read: time for its client to hear it
/// finished, catch up after a restart, and read it a page at a time.
pub const ANSWERS_KEPT_FOR: Duration = Duration::from_secs(15 * 60);

/// Jobs running at once: v26's benchmark holds a 500-position account's
/// inputs, about 25 MB, while it runs, and a 1,000-account batch about ten
/// times that; four of those beside the answers kept stay within the
/// coordinator's 2 GiB.
pub const JOBS_AT_ONCE: usize = 4;

/// Jobs waiting for one running to finish, each holding its request alone.
pub const JOBS_WAITING: usize = 64;

/// A finished job's answer, read only by the instance that ran it: kept
/// encoded, so what it holds is what it is counted at.
struct Finished {
    client: String,
    answer: Result<Arc<[u8]>, String>,
    /// When it was kept, by the coordinator's clock.
    kept_at_ns: i64,
}

impl Finished {
    fn bytes(&self) -> usize {
        match &self.answer {
            Ok(encoded) => encoded.len(),
            Err(refused) => refused.len(),
        }
    }
}

#[derive(Default)]
struct Jobs {
    answers: BTreeMap<String, Finished>,
    order: VecDeque<String>,
    /// What the answers kept hold, as [`Finished::bytes`] counts them.
    kept_bytes: usize,
    events: BTreeMap<String, Vec<JobFinishedEvent>>,
    sequences: BTreeMap<String, i64>,
}

impl Jobs {
    /// The oldest answers left, while more are kept than the count, the
    /// bytes or the age allow.
    fn trim(&mut self, config: &Config, now_ns: i64) {
        let oldest_kept = now_ns.saturating_sub(config.answers_kept_for.as_nanos() as i64);
        while let Some(oldest) = self.order.front() {
            let expired = self
                .answers
                .get(oldest)
                .is_none_or(|held| held.kept_at_ns < oldest_kept);
            if !expired
                && self.order.len() <= config.answers_kept
                && self.kept_bytes <= config.answers_kept_bytes
            {
                break;
            }
            let oldest = self.order.pop_front().expect("one is first");
            if let Some(gone) = self.answers.remove(&oldest) {
                self.kept_bytes -= gone.bytes();
            }
        }
    }
}

/// The jobs running and waiting, so that only so many hold their inputs at
/// once.
#[derive(Default)]
struct Slots {
    running: usize,
    waiting: usize,
}

/// A job's place among those running, given back when it finishes, however
/// it finishes.
pub(crate) struct Slot<'a> {
    coordinator: &'a Coordinator,
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        let mut slots = self.coordinator.slots.lock().expect("not poisoned");
        slots.running -= 1;
        self.coordinator.slot_freed.notify_one();
    }
}

#[derive(Default)]
struct Batches {
    waiting: BTreeMap<String, Vec<PriceBatchRequest>>,
    sequences: BTreeMap<String, i64>,
    answered: BTreeMap<String, PriceBatchReply>,
}

pub struct Coordinator {
    pub(crate) bus: Arc<Bus>,
    pub(crate) handle: tokio::runtime::Handle,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) config: Config,
    executor: Arc<dyn Executor>,
    jobs: Mutex<Jobs>,
    slots: Mutex<Slots>,
    slot_freed: Condvar,
    batches: Mutex<Batches>,
    batch_answered: Condvar,
    /// Contract v30 (W12.4): the standing jobs this coordinator holds, told
    /// when one's answer is in and when one is dirtied.
    pub(crate) standing: Mutex<standing::Registry>,
    pub(crate) standing_changed: Condvar,
    /// The live coordinators, and how one is asked (question 13).
    pub(crate) peers: RwLock<Arc<dyn Peers>>,
    /// A budget the standing answers reserve against in place of the
    /// process's: a test's.
    pub(crate) standing_budget: RwLock<Option<Arc<meridian_budget::Budget>>>,
}

/// The store reads for one job, each stamped with the client and its scope.
pub(crate) struct OnBehalf {
    pub(crate) bus: Arc<Bus>,
    pub(crate) handle: tokio::runtime::Handle,
    pub(crate) stamp: Stamp,
    pub(crate) timeout: Duration,
}

impl Ask for OnBehalf {
    fn ask(&self, topic: &str, kind: &str, payload: Vec<u8>) -> Result<(String, Vec<u8>), String> {
        self.handle
            .block_on(self.bus.call_stamped(
                topic,
                kind,
                payload,
                None,
                Some(self.timeout),
                &self.stamp,
            ))
            .map_err(|failed| failed.to_string())
    }

    fn client(&self) -> String {
        self.stamp.on_behalf_of_instance_id.clone()
    }
}

fn decode<M: Message + Default>(envelope: &Envelope, wanted: &str) -> Result<M, String> {
    if envelope.payload_type != wanted {
        return Err(format!("expected {wanted}, got {}", envelope.payload_type));
    }
    M::decode(&envelope.payload[..]).map_err(|failed| format!("undecodable {wanted}: {failed}"))
}

/// The client: the instance that asked, and the read scope its sidecar
/// stamped, None for a core component reading as itself.
pub(crate) fn client_of(envelope: &Envelope) -> (String, Option<Vec<String>>) {
    let meta = envelope.meta.clone().unwrap_or_default();
    let scope = meta.account_scope_applies.then_some(meta.account_scope);
    (meta.publisher_instance_id, scope)
}

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// `JOB-` and a ULID, never reused (W12.1).
pub fn job_id(now_ns: i64) -> String {
    use rand::RngCore;
    let mut random = [0u8; 10];
    rand::rngs::OsRng.fill_bytes(&mut random);
    let mut padded = [0u8; 16];
    padded[6..].copy_from_slice(&random);
    let encode = |mut value: u128, length: usize| {
        let mut out = vec![0u8; length];
        for slot in out.iter_mut().rev() {
            *slot = ALPHABET[(value & 0x1F) as usize];
            value >>= 5;
        }
        String::from_utf8(out).expect("ascii")
    };
    format!(
        "JOB-{}{}",
        encode((now_ns.max(0) / 1_000_000) as u128, 10),
        encode(u128::from_be_bytes(padded), 16)
    )
}

impl Coordinator {
    pub fn new(
        bus: Arc<Bus>,
        handle: tokio::runtime::Handle,
        clock: Arc<dyn Clock>,
        config: Config,
        executor: Arc<dyn Executor>,
    ) -> Arc<Coordinator> {
        Arc::new(Coordinator {
            bus,
            handle,
            clock,
            config,
            executor,
            jobs: Mutex::new(Jobs::default()),
            slots: Mutex::new(Slots::default()),
            slot_freed: Condvar::new(),
            batches: Mutex::new(Batches::default()),
            batch_answered: Condvar::new(),
            standing: Mutex::new(standing::Registry::default()),
            standing_changed: Condvar::new(),
            standing_budget: RwLock::new(None),
            peers: RwLock::new(Arc::new(standing::Alone(standing::member_named(
                crate::INSTANCE,
            )))),
        })
    }

    /// W12.1: run a job, answered inline or later.
    pub fn run_job(
        self: &Arc<Self>,
        request: RunJobRequest,
        envelope: &Envelope,
    ) -> Result<RunJobReply, String> {
        let (client, scope) = client_of(envelope);
        self.run_job_for(request, client, scope, false)
    }

    /// W12.1: a job run for `client` within `scope`; `handed`, asked by
    /// another coordinator, which a standing job is never handed on from.
    pub(crate) fn run_job_for(
        self: &Arc<Self>,
        request: RunJobRequest,
        client: String,
        scope: Option<Vec<String>>,
        handed: bool,
    ) -> Result<RunJobReply, String> {
        let job = request.job.unwrap_or_default();
        if let Some(scope) = &scope {
            for (i, account) in job.account_ids.iter().enumerate() {
                if !scope.contains(account) {
                    return Err(refusal_with_status(
                        RefusalStatus::PermissionDenied,
                        &[format!("job.account_ids[{i}]")],
                        format!("{account} is not in this client's read scope"),
                    ));
                }
            }
        }
        // Contract v30 (W12.1): a standing job's own refusals first.
        standing::refused(&job)?;
        let compiled = compile::compile(&job).map_err(|refused| {
            refusal_with_status(
                RefusalStatus::InvalidArgument,
                &[format!("job.{}", refused.field)],
                refused.to_string(),
            )
        })?;
        if job.standing {
            return self.run_standing(job, client, scope, handed);
        }
        // A place among the jobs running, or among those waiting for one;
        // past both, refused, never another thread holding its inputs.
        {
            let mut slots = self.slots.lock().expect("not poisoned");
            if slots.running >= self.config.jobs_at_once.max(1)
                && slots.waiting >= self.config.jobs_waiting
            {
                return Err(refusal_with_status(
                    RefusalStatus::FailedPrecondition,
                    &[],
                    format!(
                        "the analytics component is running {} jobs with {} waiting, the most \
                         it holds at once: run the job again shortly",
                        slots.running, slots.waiting
                    ),
                ));
            }
            slots.waiting += 1;
        }
        let job_id = job_id(self.clock.now_ns());
        let (told, heard) = mpsc::channel();
        let me = Arc::clone(self);
        let id = job_id.clone();
        let asker = OnBehalf {
            bus: Arc::clone(&self.bus),
            handle: self.handle.clone(),
            stamp: Stamp {
                account_scope: scope.clone(),
                on_behalf_of_instance_id: client.clone(),
                ..Stamp::default()
            },
            timeout: self.config.read_timeout,
        };
        let finishing_client = client.clone();
        std::thread::spawn(move || {
            let slot = me.slot();
            let answered = me.answer(&asker, &job, &compiled, &id);
            drop(slot);
            if told.send(answered.clone()).is_err() {
                // The reply went: the job finishes later (W12.4).
                me.finished(&id, &finishing_client, answered);
            }
        });
        match heard.recv_timeout(self.config.inline) {
            Ok(Ok(answer)) if crate::pages::whole_fits(&answer, self.config.page_budget) => {
                self.keep(&job_id, &client, Ok(answer.clone()));
                Ok(RunJobReply {
                    answered_inline: true,
                    job_id,
                    answer: Some(answer),
                })
            }
            // Past a page's budget, never inline: the client is told it
            // finished and reads it in pages (W12.1's choice by size, Q5;
            // ruling 794), so a reply's answer is always whole.
            Ok(Ok(answer)) => {
                self.finished(&job_id, &client, Ok(answer));
                Ok(RunJobReply {
                    answered_inline: false,
                    job_id,
                    answer: None,
                })
            }
            Ok(Err(refused)) => Err(refused),
            Err(_) => Ok(RunJobReply {
                answered_inline: false,
                job_id,
                answer: None,
            }),
        }
    }

    /// A place among the jobs running: waits, counted among those waiting,
    /// until one is free.
    /// Contract v30 (W12.4): a standing job's recomputation's place among
    /// the jobs running, as a job's: waits until one is free, never refused
    /// -- its holders keep the figures they have meanwhile.
    pub(crate) fn standing_slot(&self) -> Slot<'_> {
        self.slots.lock().expect("not poisoned").waiting += 1;
        self.slot()
    }

    fn slot(&self) -> Slot<'_> {
        let mut slots = self.slots.lock().expect("not poisoned");
        while slots.running >= self.config.jobs_at_once.max(1) {
            slots = self.slot_freed.wait(slots).expect("not poisoned");
        }
        slots.waiting -= 1;
        slots.running += 1;
        Slot { coordinator: self }
    }

    /// A job's answer: its inputs resolved, its routed batches priced, its
    /// units run.
    fn answer(
        &self,
        asker: &dyn Ask,
        job: &meridian_domain::v1::Job,
        compiled: &compile::Compiled,
        job_id: &str,
    ) -> Result<JobAnswer, String> {
        self.answer_read(asker, job, compiled, job_id)
            .map(|(answer, _)| answer)
    }

    /// A job's answer, and the inputs document it was computed from.
    pub(crate) fn answer_read(
        &self,
        asker: &dyn Ask,
        job: &meridian_domain::v1::Job,
        compiled: &compile::Compiled,
        job_id: &str,
    ) -> Result<(JobAnswer, serde_json::Value), String> {
        let resolved = resolve::resolve(
            asker,
            job,
            compiled,
            &self.config.engine_version,
            self.clock.now_ns(),
        )?;
        let mut inputs = resolved.inputs;
        if !resolved.routes.is_empty() {
            self.price_routed(&mut inputs, &resolved.routes)?;
        }
        let answer = meridian_analytics_engine::compute_with(&inputs, self.executor.as_ref())
            .map_err(|failed| failed.0)?;
        if answer.asks_execution_cost {
            self.record_execution_costs(&wire::execution_costs(&answer, &resolved.executions));
        }
        Ok((wire::job_answer(job_id, &answer)?, inputs))
    }

    /// Contract v27 (W10.11; ruling 481): the rows an `execution_cost` job
    /// answered, recorded in the lake as the component itself, 500 a call;
    /// the same inputs again a repeat, a different value the next version.
    /// The answer stands whether or not the lake took them: a failure is
    /// said, and the next job records them.
    fn record_execution_costs(&self, costs: &[meridian_domain::v1::ExecutionCost]) {
        for batch in costs.chunks(500) {
            let request = meridian_domain::v1::RecordExecutionCostsRequest {
                costs: batch.to_vec(),
            };
            let recorded = self.handle.block_on(self.bus.call(
                topics::RECORD_EXECUTION_COSTS,
                "meridian.v1.RecordExecutionCostsRequest",
                request.encode_to_vec(),
                None,
                Some(self.config.read_timeout),
            ));
            if let Err(failed) = recorded {
                tracing::warn!(%failed, rows = batch.len(), "execution costs were not recorded");
            }
        }
    }

    /// A finished answer kept to be read, encoded, within the count, the
    /// bytes and the age the coordinator keeps (Q12: losable).
    fn keep(&self, job_id: &str, client: &str, answer: Result<JobAnswer, String>) {
        let answer = match answer {
            Ok(answer) if answer.encoded_len() > self.config.answers_kept_bytes => {
                Err(refusal_with_status(
                    RefusalStatus::FailedPrecondition,
                    &[],
                    format!(
                        "{job_id}'s answer is {} bytes, more than the {} the analytics \
                         component keeps for every job's answers: narrow the job",
                        answer.encoded_len(),
                        self.config.answers_kept_bytes
                    ),
                ))
            }
            Ok(answer) => Ok(Arc::from(answer.encode_to_vec())),
            Err(refused) => Err(refused),
        };
        let now_ns = self.clock.now_ns();
        let finished = Finished {
            client: client.to_string(),
            answer,
            kept_at_ns: now_ns,
        };
        let mut jobs = self.jobs.lock().expect("not poisoned");
        jobs.kept_bytes += finished.bytes();
        if let Some(replaced) = jobs.answers.insert(job_id.to_string(), finished) {
            jobs.kept_bytes -= replaced.bytes();
        } else {
            jobs.order.push_back(job_id.to_string());
        }
        jobs.trim(&self.config, now_ns);
    }

    /// A job answered later has finished: its answer kept, and the client
    /// told, sequenced for its catch-up (W12.4). The sequence never goes
    /// back (ruling 973): it is at least the clock's milliseconds, so a
    /// coordinator restarted, its sequences lost, numbers past what its
    /// clients last saw, and a client's catch-up after the last it saw
    /// hears every job finished since. Counted from 0 again, a plugin that
    /// outlived the restart asked after a sequence the coordinator would not
    /// reach for hundreds of jobs, and every job it waited on timed out
    /// (dev, 2026-10-31).
    fn finished(&self, job_id: &str, client: &str, answer: Result<JobAnswer, String>) {
        let answer_hash = answer
            .as_ref()
            .ok()
            .and_then(|a| a.provenance.as_ref())
            .map(|p| p.answer_hash.clone())
            .unwrap_or_default();
        self.keep(job_id, client, answer);
        self.tell(job_id, client, answer_hash);
    }

    /// The client told a job it ran has finished, sequenced for its
    /// catch-up (W12.4): a one-off job's, and from contract v30 a standing
    /// job's first answer, where its `RunJob` was answered before it was in.
    pub(crate) fn tell(&self, job_id: &str, client: &str, answer_hash: String) {
        let event = {
            let mut jobs = self.jobs.lock().expect("not poisoned");
            let now_ms = self.clock.now_ns() / 1_000_000;
            let sequence = jobs.sequences.entry(client.to_string()).or_insert(0);
            *sequence = (*sequence + 1).max(now_ms);
            let event = JobFinishedEvent {
                job_id: job_id.to_string(),
                client_instance_id: client.to_string(),
                answer_hash,
                finished_at_ns: self.clock.now_ns(),
                sequence: *sequence,
            };
            let kept = jobs.events.entry(client.to_string()).or_default();
            kept.push(event.clone());
            let most = self.config.answers_kept;
            if kept.len() > most {
                kept.drain(..kept.len() - most);
            }
            event
        };
        if let Err(failed) = self.bus.publish(
            topics::JOB_FINISHED,
            "meridian.v1.JobFinishedEvent",
            event.encode_to_vec(),
            None,
            None,
        ) {
            tracing::warn!(%failed, job_id, "a finished job could not be told");
        }
    }

    /// W12.4: a finished job's answer, read by the instance that ran it,
    /// one page at a time (contract v28's amendment, ruling 794).
    pub fn read_job_answer(
        &self,
        request: ReadJobAnswerRequest,
        envelope: &Envelope,
    ) -> Result<JobAnswer, String> {
        let (client, scope) = client_of(envelope);
        self.read_job_answer_for(request, &client, &scope, false)
    }

    /// W12.4: a job's answer read for `client`: a standing job's held here,
    /// a one-off job's kept here, or, not `handed`, either held by another
    /// coordinator (question 13).
    pub(crate) fn read_job_answer_for(
        &self,
        request: ReadJobAnswerRequest,
        client: &str,
        scope: &Option<Vec<String>>,
        handed: bool,
    ) -> Result<JobAnswer, String> {
        if let Some(read) = self.read_standing(&request, client) {
            return read;
        }
        let held_here = self
            .jobs
            .lock()
            .expect("not poisoned")
            .answers
            .contains_key(&request.job_id);
        if !held_here && !handed {
            if let Some(read) = self.read_elsewhere(&request, client, scope) {
                return read;
            }
        }
        let client = client.to_string();
        let mut jobs = self.jobs.lock().expect("not poisoned");
        jobs.trim(&self.config, self.clock.now_ns());
        match jobs.answers.get(&request.job_id) {
            Some(held) if held.client == client => match &held.answer {
                Ok(encoded) => {
                    // Decoded for the page outside the lock: one reader's
                    // answer never holds up another's.
                    let encoded = Arc::clone(encoded);
                    drop(jobs);
                    let answer = JobAnswer::decode(&encoded[..])
                        .map_err(|failed| format!("a kept answer did not decode: {failed}"))?;
                    crate::pages::page(&answer, &request, self.config.page_budget)
                }
                Err(refused) => Err(refused.clone()),
            },
            // Another instance's job is not this caller's to know of: answered
            // as one not held (the fixture's NOT_FOUND), never naming whose.
            Some(_) => Err(refusal_with_status(
                RefusalStatus::NotFound,
                &["job_id".to_string()],
                format!(
                    "{} is not held for this instance: it is still running, or it is not \
                     one this instance ran",
                    request.job_id
                ),
            )),
            None if !request.cursor.is_empty() => Err(refusal_with_status(
                RefusalStatus::NotFound,
                &["job_id".to_string()],
                format!(
                    "{} is not held: its answer left the component's cache between pages, \
                     and the job is run again (Q12)",
                    request.job_id
                ),
            )),
            None => Err(refusal_with_status(
                RefusalStatus::NotFound,
                &["job_id".to_string()],
                format!(
                    "{} is not held: it is still running, or its answer has left the \
                     component's cache and the job is run again (Q12)",
                    request.job_id
                ),
            )),
        }
    }

    /// W12.4: the jobs this instance ran that finished after a sequence.
    pub fn list_finished_jobs(
        &self,
        request: ListFinishedJobsRequest,
        envelope: &Envelope,
    ) -> ListFinishedJobsReply {
        let (client, _) = client_of(envelope);
        let jobs = self.jobs.lock().expect("not poisoned");
        let finished: Vec<JobFinishedEvent> = jobs
            .events
            .get(&client)
            .map(|events| {
                events
                    .iter()
                    .filter(|e| e.sequence > request.after_sequence)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        ListFinishedJobsReply {
            sequence: jobs.sequences.get(&client).copied().unwrap_or(0),
            finished,
        }
    }

    /// W12.3: each routed model id's batch handed to its plugin, once, and
    /// its answers taken into the document; a plugin that does not answer
    /// by the deadline leaves its figures not evaluated (requirement 22).
    fn price_routed(
        &self,
        inputs: &mut Value,
        routes: &BTreeMap<String, String>,
    ) -> Result<(), String> {
        let case = Case::from_json(inputs).map_err(|failed| failed.0)?;
        let mut routed = Vec::new();
        let mut priced = Vec::new();
        for (model_id, instance) in routes {
            let mut units = Vec::new();
            let mut seen = std::collections::BTreeSet::new();
            for position in &case.positions {
                let Some(record) = case.instruments.get(&position.instrument_id) else {
                    continue;
                };
                if &record.model_id != model_id {
                    continue;
                }
                let Ok(base) = case.base(&position.account_id) else {
                    continue;
                };
                if !seen.insert((position.instrument_id.clone(), base.to_string())) {
                    continue;
                }
                let Some(objects) = pricing::unit_inputs(&case, &position.instrument_id, base)
                else {
                    continue;
                };
                units.push(PricingUnit {
                    instrument_id: position.instrument_id.clone(),
                    template_version: record.template_version.clone(),
                    scenario_key: Some(ScenarioKey {
                        kind: ScenarioKind::Base as i32,
                        ..Default::default()
                    }),
                    inputs: objects
                        .iter()
                        .map(|object| MarketObjectValue {
                            subject_id: object.subject_id.clone(),
                            kind: object.kind.clone(),
                            value: object
                                .amount
                                .text()
                                .parse::<Exact>()
                                .ok()
                                .map(|amount| Money {
                                    amount: Some(amount.to_wire()),
                                    currency_code: String::new(),
                                    instrument_id: object.instrument_id.clone(),
                                }),
                            dataset: object.dataset.clone(),
                        })
                        .collect(),
                });
            }
            let answered = if units.is_empty() {
                Some(Vec::new())
            } else {
                self.ask_batch(instance, model_id, units)
            };
            routed.push(json!({
                "model_id": model_id,
                "implementation": instance,
                "implementation_version": "",
                "answered": answered.is_some(),
            }));
            for value in answered.unwrap_or_default() {
                let Some(money) = value.value_per_unit else {
                    continue;
                };
                let Some(amount) = money.amount.as_ref().and_then(|d| Exact::from_wire(d).ok())
                else {
                    continue;
                };
                priced.push(json!({
                    "instrument_id": value.instrument_id,
                    "base_currency_instrument_id": money.instrument_id,
                    "value_per_unit": amount.to_string(),
                }));
            }
        }
        priced.sort_by_key(|p| {
            (
                p["instrument_id"].as_str().unwrap_or_default().to_string(),
                p["base_currency_instrument_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            )
        });
        inputs["routed"] = Value::Array(routed);
        inputs["priced"] = Value::Array(priced);
        Ok(())
    }

    /// One batch handed to `instance` and waited for, to its deadline.
    fn ask_batch(
        &self,
        instance: &str,
        model_id: &str,
        units: Vec<PricingUnit>,
    ) -> Option<Vec<meridian_domain::v1::PricedUnit>> {
        let named = json!({
            "model_id": model_id,
            "units": units.iter().map(|u| json!({
                "instrument_id": u.instrument_id,
                "template_version": u.template_version,
                "inputs": u.inputs.iter().map(|i| json!({
                    "subject_id": i.subject_id,
                    "kind": i.kind,
                    "amount": i.value.as_ref().and_then(|m| m.amount.as_ref()).and_then(|d| Exact::from_wire(d).ok()).map(|e| e.to_string()),
                    "instrument_id": i.value.as_ref().map(|m| m.instrument_id.clone()),
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        });
        let batch_hash = canonical::sha256_name(
            canonical::canonical(&named)
                .expect("a batch is canonical")
                .as_bytes(),
        );
        let deadline = self.clock.now_ns() + self.config.price_deadline.as_nanos() as i64;
        let batch = {
            let mut batches = self.batches.lock().expect("not poisoned");
            // Never back across a restart, as a finished job's (ruling 973):
            // a pricing plugin catching up after the last it saw hears it.
            let now_ms = self.clock.now_ns() / 1_000_000;
            let sequence = batches.sequences.entry(instance.to_string()).or_insert(0);
            *sequence = (*sequence + 1).max(now_ms);
            let batch = PriceBatchRequest {
                batch_id: batch_hash.clone(),
                pricing_instance_id: instance.to_string(),
                model_id: model_id.to_string(),
                model_version: 1,
                units,
                deadline_ns: deadline,
                batch_hash: batch_hash.clone(),
                sequence: *sequence,
            };
            batches
                .waiting
                .entry(instance.to_string())
                .or_default()
                .push(batch.clone());
            batch
        };
        if let Err(failed) = self.bus.publish(
            topics::PRICE_BATCH,
            "meridian.v1.PriceBatchRequest",
            batch.encode_to_vec(),
            None,
            None,
        ) {
            tracing::warn!(%failed, instance, "a price batch could not be handed over");
        }
        // Waited on the monotonic timer, never the deployment's clock: how
        // long to wait is not a time anything records.
        let until = std::time::Instant::now() + self.config.price_deadline;
        let mut batches = self.batches.lock().expect("not poisoned");
        let answered = loop {
            if let Some(reply) = batches.answered.remove(&batch_hash) {
                break Some(reply.values);
            }
            let left = until.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break None;
            }
            let (held, _) = self
                .batch_answered
                .wait_timeout(batches, left)
                .expect("not poisoned");
            batches = held;
        };
        if let Some(waiting) = batches.waiting.get_mut(instance) {
            waiting.retain(|b| b.batch_id != batch_hash);
        }
        answered
    }

    /// W12.3: the batches waiting for the asking instance after a sequence.
    pub fn list_price_batches(
        &self,
        request: ListPriceBatchesRequest,
        envelope: &Envelope,
    ) -> ListPriceBatchesReply {
        let (instance, _) = client_of(envelope);
        let batches = self.batches.lock().expect("not poisoned");
        ListPriceBatchesReply {
            batches: batches
                .waiting
                .get(&instance)
                .map(|waiting| {
                    waiting
                        .iter()
                        .filter(|b| b.sequence > request.after_sequence)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default(),
            sequence: batches.sequences.get(&instance).copied().unwrap_or(0),
        }
    }

    /// W12.3: a `pricing` plugin's answer to a batch handed to it. A batch
    /// answered twice, or after its deadline, is harmless: named by its
    /// hash, it is taken once or not at all.
    pub fn answer_price_batch(
        &self,
        reply: PriceBatchReply,
        envelope: &Envelope,
    ) -> Result<AnswerPriceBatchReply, String> {
        let (sender, _) = client_of(envelope);
        let answering = if reply.pricing_instance_id.is_empty() {
            sender
        } else {
            reply.pricing_instance_id.clone()
        };
        let mut batches = self.batches.lock().expect("not poisoned");
        let handed = batches
            .waiting
            .get(&answering)
            .is_some_and(|waiting| waiting.iter().any(|b| b.batch_id == reply.batch_id));
        if !handed {
            return Err(refusal_with_status(
                RefusalStatus::InvalidArgument,
                &["batch_id".to_string()],
                format!(
                    "batch_id: {} is not a batch waiting for {answering}",
                    reply.batch_id
                ),
            ));
        }
        let batch_id = reply.batch_id.clone();
        batches.answered.insert(batch_id.clone(), reply);
        self.batch_answered.notify_all();
        Ok(AnswerPriceBatchReply { batch_id })
    }
}

fn serve_on<Req, Rep, F>(
    bus: &Arc<Bus>,
    coordinator: &Arc<Coordinator>,
    topic: &'static str,
    types: (&'static str, &'static str),
    handle: F,
) where
    Req: Message + Default,
    Rep: Message,
    F: Fn(&Arc<Coordinator>, Req, &Envelope) -> Result<Rep, String> + Send + Sync + 'static,
{
    let (request_type, reply_type) = types;
    let coordinator = Arc::clone(coordinator);
    bus.serve(topic, move |envelope| {
        let request: Req = decode(&envelope, request_type)?;
        let reply = handle(&coordinator, request, &envelope)?;
        Ok((reply_type.to_string(), reply.encode_to_vec()))
    });
}

/// Register the coordinator's handlers.
pub fn serve(bus: &Arc<Bus>, coordinator: &Arc<Coordinator>) {
    serve_on(
        bus,
        coordinator,
        topics::RUN_JOB,
        ("meridian.v1.RunJobRequest", "meridian.v1.RunJobReply"),
        |c, request: RunJobRequest, envelope| c.run_job(request, envelope),
    );
    serve_on(
        bus,
        coordinator,
        topics::READ_JOB_ANSWER,
        ("meridian.v1.ReadJobAnswerRequest", "meridian.v1.JobAnswer"),
        |c, request: ReadJobAnswerRequest, envelope| c.read_job_answer(request, envelope),
    );
    serve_on(
        bus,
        coordinator,
        topics::LIST_FINISHED_JOBS,
        (
            "meridian.v1.ListFinishedJobsRequest",
            "meridian.v1.ListFinishedJobsReply",
        ),
        |c, request: ListFinishedJobsRequest, envelope| Ok(c.list_finished_jobs(request, envelope)),
    );
    serve_on(
        bus,
        coordinator,
        topics::LIST_MEASURE_DEFINITIONS,
        (
            "meridian.v1.ListMeasureDefinitionsRequest",
            "meridian.v1.ListMeasureDefinitionsReply",
        ),
        |_, _request: ListMeasureDefinitionsRequest, _| {
            Ok(ListMeasureDefinitionsReply {
                definitions: definitions::wire(),
            })
        },
    );
    serve_on(
        bus,
        coordinator,
        topics::LIST_PRICE_BATCHES,
        (
            "meridian.v1.ListPriceBatchesRequest",
            "meridian.v1.ListPriceBatchesReply",
        ),
        |c, request: ListPriceBatchesRequest, envelope| Ok(c.list_price_batches(request, envelope)),
    );
    serve_on(
        bus,
        coordinator,
        topics::ANSWER_PRICE_BATCH,
        (
            "meridian.v1.PriceBatchReply",
            "meridian.v1.AnswerPriceBatchReply",
        ),
        |c, reply: PriceBatchReply, envelope| c.answer_price_batch(reply, envelope),
    );
    // Contract v30 (W12.4): a sidecar's renewal of its standing jobs.
    crate::standing::coordinator::serve_renewals(bus, coordinator);
}

/// The executor the coordinator answers with: the worker pool over the bus.
pub fn pool(
    bus: &Arc<Bus>,
    handle: tokio::runtime::Handle,
    timeout: Duration,
) -> Arc<dyn Executor> {
    pool_queued(bus, handle, timeout, None)
}

/// The same, its units put on the JetStream work queue where there is one
/// (step 3b, [`crate::queue`]).
pub fn pool_queued(
    bus: &Arc<Bus>,
    handle: tokio::runtime::Handle,
    timeout: Duration,
    queue: Option<Arc<crate::queue::Units>>,
) -> Arc<dyn Executor> {
    Arc::new(crate::worker::Pool {
        bus: Arc::clone(bus),
        handle,
        timeout,
        queue,
    })
}

/// The engine's own executor, for a coordinator with no pool: a test, or
/// one process serving as both.
pub fn local() -> Arc<dyn Executor> {
    Arc::new(graph::Sequential)
}
