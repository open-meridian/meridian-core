//! A member of a component: its writer, or a standby waiting to take over
//! (step 3b; spec/a-deployment-scales-out-and-fails-over-by-configuration
//! sections 1 to 3; plan Track 5c; rulings 1046, 1047, 1051, 1052, 1056).
//!
//! **One image, in a mode.** Every member of a store -- the lake, the book,
//! the instrument store, the street store, the conductor -- and the launcher
//! is the same binary. With no lease configured (`MERIDIAN_LEASE_NAME`
//! unset: the tolerant default, one member) it writes from the start, as it
//! always has, and recovers by restart. With one (`<store>.standby` above 0)
//! each member starts as a **standby**: it connects, verifies its schema,
//! holds no lease, serves nothing, and follows its writer's change log --
//! ready only while its lag is under `<store>.readyLagMs` -- until it takes
//! the lease. Then it raises the epoch on its partitions (the store's
//! `take_over`, which fences the old writer's next commit), records that it
//! took over, and serves as the writer. A writer restarted comes back as the
//! standby.
//!
//! **Stepping down.** A writer stops writing, by ending its process, as soon
//! as it learns it may no longer: another member holds the lease, it could
//! not renew within its deadline (before the lease ends, so no standby
//! overlaps it), its store's epoch moved past its own, or a commit of its own
//! was refused by a newer epoch. Its pod restarts it, and it comes back as
//! the standby. Whatever it believed in between, the epoch refused its
//! commits: the lease is liveness, the epoch the fence.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use meridian_bus::Bus;
use prost::Message as _;

use crate::lease::{Elector, KubeLeases, Settings, Standing};
use crate::{now_ns, var};

/// The exit a member steps down with: its pod's restart brings it back as
/// the standby.
pub const STEPPED_DOWN: i32 = 75;

/// A member's mode, as it reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Writer,
    Standby,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Writer => "writer",
            Mode::Standby => "standby",
        }
    }
}

/// What this member says of itself in its `ComponentReport`: working state,
/// never a record (decisions/031).
#[derive(Default)]
pub struct Said {
    mode: Mutex<String>,
    lag_ms: AtomicU64,
    partitions: Mutex<Vec<String>>,
}

impl Said {
    pub fn mode(&self) -> String {
        self.mode.lock().expect("said poisoned").clone()
    }
    pub fn lag_ms(&self) -> u64 {
        self.lag_ms.load(Ordering::Relaxed)
    }
    pub fn partitions(&self) -> Vec<String> {
        self.partitions.lock().expect("said poisoned").clone()
    }
    pub fn set_mode(&self, mode: Mode) {
        *self.mode.lock().expect("said poisoned") = mode.as_str().to_string();
    }
    pub fn set_lag_ms(&self, lag: u64) {
        self.lag_ms.store(lag, Ordering::Relaxed);
    }
    /// The partitions it writes or follows, sorted, at most 64 named.
    pub fn set_partitions(&self, mut partitions: Vec<String>) {
        partitions.sort();
        partitions.dedup();
        partitions.truncate(64);
        *self.partitions.lock().expect("said poisoned") = partitions;
    }
}

/// This process's member, as its reports read it.
pub fn said() -> &'static Said {
    static SAID: OnceLock<Said> = OnceLock::new();
    SAID.get_or_init(Said::default)
}

/// This member's name (its pod's) and zone, from the downward API.
pub fn member_name() -> String {
    var("MERIDIAN_POD_NAME").unwrap_or_default()
}

pub fn zone() -> String {
    var("MERIDIAN_ZONE").unwrap_or_default()
}

/// A member's settings, from what the chart sets.
pub struct Member {
    component: &'static str,
    /// The change log's domain it follows as a standby; none for the
    /// launcher, which keeps no store.
    domain: Option<&'static str>,
    lease: Option<Settings>,
    ready_lag: Duration,
}

/// What a standby's lag is held under by default where the chart says
/// nothing: five seconds, a tail far behind any healthy writer's.
pub const READY_LAG_DEFAULT_MS: u64 = 5_000;

impl Member {
    /// `MERIDIAN_LEASE_NAME` names the lease (unset: no lease, one member);
    /// `MERIDIAN_POD_NAME` this member; `MERIDIAN_LEASE_RENEW_SECONDS` and
    /// `MERIDIAN_LEASE_DURATION_SECONDS` (2 and 6) its timing;
    /// `MERIDIAN_READY_LAG_MS` the lag a standby is ready under.
    pub fn from_env(component: &'static str, domain: Option<&'static str>) -> Result<Self, String> {
        let whole = |name: &str, default: u64| -> Result<u64, String> {
            match var(name) {
                None => Ok(default),
                Some(said) => said
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| format!("{name} is {said:?}; a whole number")),
            }
        };
        let lease = match var("MERIDIAN_LEASE_NAME") {
            None => None,
            Some(name) => {
                // This process, not only its pod: a writer's container
                // restarted in the same pod is a new member, which comes back
                // as the standby (spec section 3) rather than finding its pod's
                // name on the lease and taking it back as its own. Pod names
                // never hold an underscore.
                let holder = format!("{}_{:x}", member_name(), now_ns());
                let settings = Settings {
                    name,
                    holder,
                    renew: Duration::from_secs(whole("MERIDIAN_LEASE_RENEW_SECONDS", 2)?),
                    duration: Duration::from_secs(whole("MERIDIAN_LEASE_DURATION_SECONDS", 6)?),
                };
                settings.check()?;
                Some(settings)
            }
        };
        Ok(Self {
            component,
            domain,
            lease,
            ready_lag: Duration::from_millis(whole("MERIDIAN_READY_LAG_MS", READY_LAG_DEFAULT_MS)?),
        })
    }

    /// A member with no lease: what tests and a local stack run.
    pub fn sole(component: &'static str) -> Self {
        Self {
            component,
            domain: None,
            lease: None,
            ready_lag: Duration::from_millis(READY_LAG_DEFAULT_MS),
        }
    }

    pub fn leased(&self) -> bool {
        self.lease.is_some()
    }

    /// Wait until this member may write.
    ///
    /// With no lease, at once: the one member writes, as it always has. With
    /// one, as a standby: following its writer's change log, ready while its
    /// lag is under its bound, looking at the lease twice a renew, until it
    /// takes it. The member reports as a standby meanwhile.
    pub async fn wait_to_write(&self, bus: &Arc<Bus>) -> Result<Holding, String> {
        let Some(settings) = self.lease.clone() else {
            said().set_mode(Mode::Writer);
            return Ok(Holding {
                elector: None,
                component: self.component,
                why: "",
            });
        };
        said().set_mode(Mode::Standby);
        tokio::spawn(report_forever(Arc::clone(bus), self.component));
        let api = KubeLeases::in_cluster(settings.renew)?;
        let look_every = settings.look_every();
        let mut elector = Elector::new(api, settings);
        let following = self
            .domain
            .map(|domain| tokio::spawn(follow(Arc::clone(bus), domain, self.ready_lag)));
        if following.is_none() {
            // Nothing to follow: a standby of the launcher is ready while it
            // waits.
            crate::mark_ready(true);
        }
        let mut said_waiting = false;
        loop {
            match elector.look(Instant::now(), now_ns()).await {
                Ok(Standing::Held) => break,
                Ok(Standing::HeldBy(holder)) => {
                    if !said_waiting {
                        tracing::info!(
                            component = self.component,
                            writer = %holder,
                            "a standby: following the writer until its lease lapses"
                        );
                        said_waiting = true;
                    }
                }
                Err(failed) => {
                    tracing::warn!(component = self.component, %failed, "the lease could not be read; looking again");
                }
            }
            tokio::time::sleep(look_every).await;
        }
        if let Some(following) = following {
            following.abort();
        }
        crate::mark_ready(false);
        said().set_lag_ms(0);
        said().set_mode(Mode::Writer);
        tracing::info!(
            component = self.component,
            member = %member_name(),
            "this member holds the lease: taking the writer's partitions over"
        );
        let why = match elector.took_from() {
            Some(Some(holder)) if !holder.is_empty() => meridian_changes::LEASE_LAPSED,
            Some(Some(_)) => meridian_changes::LEASE_RELEASED,
            _ => meridian_changes::LEASE_FREE,
        };
        Ok(Holding {
            elector: Some(Arc::new(tokio::sync::Mutex::new(elector))),
            component: self.component,
            why,
        })
    }
}

/// A writer's hold on its lease, from taking it to stopping.
pub struct Holding {
    elector: Option<Arc<tokio::sync::Mutex<Elector<KubeLeases>>>>,
    component: &'static str,
    why: &'static str,
}

impl Holding {
    /// Whether this writer took a lease, and so takes the partitions over
    /// (raising their epoch) rather than adopting the epoch it finds.
    pub fn took_over(&self) -> bool {
        self.elector.is_some()
    }

    /// The epoch this writer writes under: the one it finds where it is the
    /// only member (no lease), or a new one where it took a lease -- its
    /// store's partitions taken over, the old writer fenced, and who took
    /// them over and why recorded (`WriterTookOver`) in the same transaction.
    /// Blocking: the store is read.
    pub fn epoch<A, T>(&self, adopt: A, take_over: T) -> Result<u64, String>
    where
        A: FnOnce() -> Result<u64, String>,
        T: FnOnce(&meridian_changes::TookOver) -> Result<u64, String>,
    {
        if !self.took_over() {
            return adopt();
        }
        let by = meridian_changes::TookOver {
            member: member_name(),
            why: self.why.to_string(),
            at_ns: now_ns(),
        };
        let epoch = take_over(&by)?;
        tracing::info!(
            component = self.component,
            epoch,
            why = self.why,
            "took the writer's partitions over: the old writer's commits are refused from here"
        );
        Ok(epoch)
    }

    /// Keep the lease while writing: renewed every renew; the member steps
    /// down when another holds it, when it could not renew within its
    /// deadline, when `epoch_now` -- the store's head epoch, read from the
    /// record -- moves past `mine`, or when a commit of its own was fenced.
    /// Nothing is kept where no lease is held.
    pub fn keep<E>(&self, mine: u64, epoch_now: E)
    where
        E: Fn() -> Result<u64, String> + Send + Sync + 'static,
    {
        let Some(elector) = self.elector.clone() else {
            return;
        };
        let component = self.component;
        let epoch_now = Arc::new(epoch_now);
        tokio::spawn(async move {
            let (renew, deadline) = {
                let held = elector.lock().await;
                (held.settings().renew, held.settings().renew_deadline())
            };
            let mut last_renewed = Instant::now();
            loop {
                tokio::time::sleep(renew).await;
                if meridian_changes::fenced_seen() {
                    step_down(
                        component,
                        "a commit of this writer was refused by a newer epoch",
                    );
                }
                let looked = elector.lock().await.look(Instant::now(), now_ns()).await;
                match looked {
                    Ok(Standing::Held) => last_renewed = Instant::now(),
                    Ok(Standing::HeldBy(holder)) => step_down(
                        component,
                        &format!("another member ({holder}) holds the lease"),
                    ),
                    Err(failed) => {
                        tracing::warn!(component, %failed, "the lease could not be renewed");
                    }
                }
                if last_renewed.elapsed() >= deadline {
                    step_down(
                        component,
                        &format!(
                            "the lease was not renewed within its deadline ({} s): \
                             stopping before it lapses, so no standby overlaps this writer",
                            deadline.as_secs()
                        ),
                    );
                }
                let reading = Arc::clone(&epoch_now);
                match tokio::task::spawn_blocking(move || reading()).await {
                    Ok(Ok(head)) if head > mine => step_down(
                        component,
                        &format!("the store's epoch is {head}, past this writer's {mine}"),
                    ),
                    Ok(Ok(_)) => {}
                    Ok(Err(failed)) => {
                        tracing::warn!(component, %failed, "the store's epoch could not be read")
                    }
                    Err(failed) => tracing::warn!(component, %failed, "the epoch check failed"),
                }
            }
        });
    }

    /// Give the lease up on a planned stop: the standby takes over at its
    /// next look rather than after the lease lapses.
    pub async fn release(&self) {
        if let Some(elector) = &self.elector {
            elector.lock().await.release(now_ns()).await;
        }
    }
}

/// Stop writing, now: not ready, and the process ended. Its pod restarts
/// it, and it comes back as the standby.
pub fn step_down(component: &str, why: &str) -> ! {
    tracing::error!(
        component,
        why,
        "stepping down: this member no longer writes"
    );
    crate::mark_ready(false);
    std::process::exit(STEPPED_DOWN);
}

/// Report as a member while standing by: the writer's own report starts
/// once it serves.
async fn report_forever(bus: Arc<Bus>, component: &'static str) {
    loop {
        if said().mode() != Mode::Standby.as_str() {
            return;
        }
        let report = member_report(component, 0, now_ns());
        let _ = bus.publish(
            crate::COMPONENT_REPORT_TOPIC,
            "meridian.v1.ComponentReport",
            report.encode_to_vec(),
            None,
            None,
        );
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

/// This member's report: the component's, with its mode, zone, lag,
/// partitions and name.
pub fn member_report(
    component: &str,
    schema: i64,
    started_at_ns: i64,
) -> meridian_domain::v1::ComponentReport {
    meridian_domain::v1::ComponentReport {
        component: component.to_string(),
        version: crate::VERSION.to_string(),
        schema_version: schema,
        health: meridian_domain::v1::ComponentHealth::Serving as i32,
        detail: String::new(),
        started_at_ns,
        mode: said().mode(),
        zone: zone(),
        lag_ms: said().lag_ms(),
        partitions: said().partitions(),
        member: member_name(),
    }
}

/// How often a standby asks its writer, which is also its interest in the
/// tail (ruling 1062).
const ASK_EVERY: Duration = Duration::from_secs(1);

/// A standby following its writer's change log: each page heard on the
/// tail applied where it follows the last, a page missed read from the
/// writer, and the writer asked every second -- for its liveness, and so it
/// keeps publishing the tail. Ready while its lag is under `bound`.
async fn follow(bus: Arc<Bus>, domain: &'static str, bound: Duration) {
    let mut tail = bus.subscribe_every(&meridian_changes::every_changes_topic(domain));
    let mut at: HashMap<String, u64> = HashMap::new();
    let mut chains: HashMap<String, meridian_changes::integrity::Chain> = HashMap::new();
    // Behind since a page was missed and not yet read; unanswered since the
    // writer last failed to answer. The lag is the longer.
    let mut gap_since: Option<Instant> = None;
    let mut unanswered_since: Option<Instant> = None;
    let mut ask = tokio::time::interval(ASK_EVERY);
    loop {
        tokio::select! {
            heard = tail.recv() => {
                let Some(delivery) = heard else { return };
                let page = match meridian_changes::read_tail(
                    &delivery.envelope.payload_type,
                    &delivery.envelope.payload,
                ) {
                    Ok(page) => page,
                    Err(failed) => {
                        tracing::warn!(domain, %failed, "a page on the tail could not be read");
                        continue;
                    }
                };
                let Some(partition) = page.changes.first().map(|c| c.partition.clone()) else {
                    continue;
                };
                // Integrity at rest (step 3b): every change it takes in order
                // verified by its digest, the book's by its chain; one that
                // does not verify is never taken, and the standby stays
                // behind, unready, saying so. A page after a gap is checked
                // as the gap is read, in order.
                let contiguous = match at.get(&partition).copied() {
                    None => true,
                    Some(held) => page.head > held && page.after_sequence == held,
                };
                if contiguous {
                    if let Err(failed) = verified(domain, &mut chains, &page.changes) {
                        tracing::error!(domain, partition, %failed, "a change did not verify; not followed");
                        gap_since.get_or_insert_with(Instant::now);
                        continue;
                    }
                }
                match at.get(&partition).copied() {
                    None => {
                        at.insert(partition, page.head);
                    }
                    Some(held) if page.head <= held => {}
                    Some(held) if page.after_sequence == held => {
                        at.insert(partition, page.head);
                    }
                    Some(held) => {
                        // A page missed: read it from the writer, never infer it.
                        gap_since.get_or_insert_with(Instant::now);
                        match catch_up(&bus, domain, &partition, held, &mut chains).await {
                            Ok(head) => {
                                at.insert(partition, head.max(page.head));
                                gap_since = None;
                            }
                            Err(failed) => tracing::warn!(domain, partition, %failed, "a page missed could not be read"),
                        }
                    }
                }
                said().set_partitions(at.keys().cloned().collect());
            }
            _ = ask.tick() => {
                match ask_writer(&bus, domain).await {
                    Ok(()) => unanswered_since = None,
                    Err(failed) => {
                        if unanswered_since.is_none() {
                            tracing::warn!(domain, %failed, "the writer's change log did not answer");
                        }
                        unanswered_since.get_or_insert_with(Instant::now);
                    }
                }
                let lag = [gap_since, unanswered_since]
                    .into_iter()
                    .flatten()
                    .map(|since| since.elapsed().as_millis() as u64)
                    .max()
                    .unwrap_or(0);
                said().set_lag_ms(lag);
                crate::mark_ready(Duration::from_millis(lag) < bound);
            }
        }
    }
}

/// Ask the writer's change log, for its control partition from its head:
/// cheap, and it is the interest that keeps the tail published.
async fn ask_writer(bus: &Bus, domain: &str) -> Result<(), String> {
    let asked = meridian_changes::ReadChangesRequest {
        partition: meridian_changes::CONTROL.to_string(),
        after_sequence: i64::MAX as u64,
        page_size: 1,
    };
    bus.call(
        &meridian_changes::read_changes_topic(domain),
        "meridian.v1.ReadChangesRequest",
        asked.encode_to_vec(),
        None,
        Some(ASK_EVERY),
    )
    .await
    .map(|_| ())
    .map_err(|failed| failed.to_string())
}

/// Read a partition's changes after `after` from the writer, page by page,
/// to its head.
/// Each change verified by its digest, and the book's by its chain too.
fn verified(
    domain: &str,
    chains: &mut HashMap<String, meridian_changes::integrity::Chain>,
    changes: &[meridian_changes::Change],
) -> Result<(), String> {
    for change in changes {
        if domain == "book" {
            chains
                .entry(change.partition.clone())
                .or_default()
                .follow(change)?;
        } else {
            meridian_changes::integrity::verify(change)?;
        }
    }
    Ok(())
}

async fn catch_up(
    bus: &Bus,
    domain: &str,
    partition: &str,
    mut after: u64,
    chains: &mut HashMap<String, meridian_changes::integrity::Chain>,
) -> Result<u64, String> {
    loop {
        let asked = meridian_changes::ReadChangesRequest {
            partition: partition.to_string(),
            after_sequence: after,
            page_size: 0,
        };
        let (_, body) = bus
            .call(
                &meridian_changes::read_changes_topic(domain),
                "meridian.v1.ReadChangesRequest",
                asked.encode_to_vec(),
                None,
                Some(Duration::from_secs(5)),
            )
            .await
            .map_err(|failed| failed.to_string())?;
        let page = meridian_changes::ReadChangesReply::decode(body.as_slice())
            .map_err(|failed| failed.to_string())?;
        verified(domain, chains, &page.changes)?;
        for change in &page.changes {
            if change.sequence <= after {
                return Err(format!(
                    "change {} came after {after}: out of order",
                    change.sequence
                ));
            }
            after = change.sequence;
        }
        if page.changes.is_empty() || after >= page.head {
            return Ok(after.max(page.head));
        }
    }
}

#[cfg(test)]
mod tests;
