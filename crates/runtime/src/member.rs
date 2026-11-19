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
//! **A reader** (`<store>.readers`, contract v30, stage B; spec section 5):
//! the same image again, in a reads-only mode the chart sets
//! (`MERIDIAN_MEMBER_MODE=reader`). It never writes and never takes the
//! lease: it serves its store's reads alone, in the store's query queue group
//! beside the writer, follows the writer's change log from the primary's
//! heads as it starts, applying each change to its hot layer ([`Apply`]),
//! and is ready only while its lag is under `<store>.readyLagMs`. A read
//! naming a `read_after` it has not applied waits up to its bound or the
//! call's deadline less a margin, then is answered from the store's primary
//! database.
//!
//! **Stepping down.** A writer stops writing, by ending its process, as soon
//! as it learns it may no longer: another member holds the lease, it could
//! not renew within its deadline (before the lease ends, so no standby
//! overlaps it), its store's epoch moved past its own, or a commit of its own
//! was refused by a newer epoch. Its pod restarts it, and it comes back as
//! the standby. Whatever it believed in between, the epoch refused its
//! commits: the lease is liveness, the epoch the fence.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use meridian_bus::Bus;
use prost::Message as _;

use crate::lease::{Elector, KubeLeases, Leases, Settings, Standing};
use crate::{now_ns, var};

/// The exit a member steps down with: its pod's restart brings it back as
/// the standby.
pub const STEPPED_DOWN: i32 = 75;

/// A member's mode, as it reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Writer,
    Standby,
    Reader,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Writer => "writer",
            Mode::Standby => "standby",
            Mode::Reader => "reader",
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
    /// What it waits on before it can do its whole job, where it waits
    /// (step 3d): reported degraded, saying why.
    waiting: Mutex<String>,
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
    /// What it waits on, empty where it waits on nothing.
    pub fn waiting(&self) -> String {
        self.waiting.lock().expect("said poisoned").clone()
    }
    /// It waits on `why` (step 3d): reported degraded until it is
    /// [`Said::done_waiting`].
    pub fn set_waiting(&self, why: impl Into<String>) {
        *self.waiting.lock().expect("said poisoned") = why.into();
    }
    pub fn done_waiting(&self) {
        self.waiting.lock().expect("said poisoned").clear();
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

/// How often a store's member says it is serving, and how recently one must
/// have said so to be waited on by a move home (step 3d; ruled 2026-11-15,
/// item 9): the lease's renewal and its duration as the chart sets them
/// (`MERIDIAN_LEASE_RENEW_SECONDS`, `MERIDIAN_LEASE_DURATION_SECONDS`; 2 and
/// 6), with or without a lease.
pub fn seen_every_and_within() -> (Duration, Duration) {
    let whole = |name: &str, default: u64| {
        var(name)
            .and_then(|said| said.trim().parse::<u64>().ok())
            .filter(|seconds| *seconds > 0)
            .unwrap_or(default)
    };
    let every = whole("MERIDIAN_LEASE_RENEW_SECONDS", 2);
    let within = whole("MERIDIAN_LEASE_DURATION_SECONDS", 6).max(every + 1);
    (Duration::from_secs(every), Duration::from_secs(within))
}

/// This member kept in its store's members for as long as it runs (step 3d;
/// `lake_member`, `book_member`; ruled 2026-11-15, item 9): a writer, a
/// standby or a reader alike, `seen` -- its own row in the store's database
/// 0, with the `revision` it runs -- now and every few seconds after. A
/// member that cannot say so is not waited on, and is told why in its log.
pub fn keep_seen<F>(store: &'static str, instance_id: &str, revision: String, seen: F)
where
    F: Fn(&str, &str, i64, i64) -> Result<(), String> + Send + Sync + 'static,
{
    let name = seen_as(instance_id);
    let started = now_ns();
    let (every, _) = seen_every_and_within();
    let seen = Arc::new(seen);
    tracing::info!(
        store,
        member = name,
        revision,
        "this member says the revision it runs"
    );
    crate::background(async move {
        loop {
            let (seen, name, revision) = (Arc::clone(&seen), name.clone(), revision.clone());
            let said = one_look_at_a_time(move || seen(&name, &revision, started, now_ns())).await;
            if let Ok(Err(failed)) = said {
                tracing::debug!(store, %failed, "this member's row was not brought up to date");
            }
            tokio::time::sleep(every).await;
        }
    });
}

/// The name this member's row is kept under in its store's members
/// ([`keep_seen`]): its pod's, or its instance's where it runs in none.
pub fn seen_as(instance_id: &str) -> String {
    match member_name() {
        name if name.is_empty() => instance_id.to_string(),
        name => name,
    }
}

/// Whether this member serves, as its readiness file says it
/// ([`crate::mark_ready`]).
static SERVING: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_serving(serving: bool) {
    SERVING.store(serving, Ordering::SeqCst);
}

/// The mode this member serves in -- `writer`, `standby` or `reader` --
/// and empty until it serves: what it says with its row in its store's
/// members (step 3d, stage T; `mode` approved 2026-11-15). A writer that
/// holds its lease and has not yet served its topics is not yet one another
/// group's writer may stop for.
pub fn serving_mode() -> String {
    if SERVING.load(Ordering::SeqCst) {
        said().mode()
    } else {
        String::new()
    }
}

/// How long a writer of a group past the first serves on, told to stop,
/// for another group's writer to serve (approved 2026-11-15): under the
/// 30 s a pod is given to stop, with time left to give its lease up.
pub const SERVES_ON_AT_MOST: Duration = Duration::from_secs(20);
/// How often it looks meanwhile.
const SERVES_ON_LOOKS_EVERY: Duration = Duration::from_millis(500);

/// Why a member told to stop stopped serving when it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServedOn {
    /// It waits on nobody: the first group's, one group's only, or not a
    /// writer.
    AtOnce,
    /// Another group's writer serves that will not stop with it.
    AnotherServes,
    /// None was seen within [`SERVES_ON_AT_MOST`].
    Bound,
}

/// A writer told to stop keeps serving until another writer group's writer
/// serves its store's public topics (step 3d, stage T; section 15; approved
/// 2026-11-15, option A).
///
/// Every group's writer serves the store's public topics and hands a write
/// on to its owner, and an upgrade rolls every group's members at once: all
/// stopped in the same seconds, nothing served them. So the first group's
/// writer stops at once, and a writer of group `mine` past it, of `count`,
/// serves on -- its lease held, its handlers answering -- until `look`
/// says another group's writer serves that will not stop with it: one
/// started after this member was told, at `told_at_ns`, or running another
/// revision. At most [`SERVES_ON_AT_MOST`]: a stop that is no rollout -- a
/// drained node, a deleted pod -- finds none, and stops then, having served
/// meanwhile. A look that fails is asked again.
///
/// `look(told_at_ns, now_ns)` reads the store's members; it blocks, and is
/// run off the runtime's own threads.
pub async fn serve_on_until_another_group_serves<F>(
    mine: u32,
    count: u32,
    writer: bool,
    told_at_ns: i64,
    look: F,
) -> ServedOn
where
    F: Fn(i64, i64) -> Result<bool, String> + Send + Sync + 'static,
{
    serve_on_within(
        mine,
        count,
        writer,
        told_at_ns,
        look,
        SERVES_ON_AT_MOST,
        SERVES_ON_LOOKS_EVERY,
    )
    .await
}

/// [`serve_on_until_another_group_serves`], for at most `at_most`, looking
/// every `every`.
///
/// A look is given what is left of `at_most` and no longer (stage T's
/// review, finding 1). It reads the store's database 0 through the store's
/// pool, which waits up to its connection timeout -- 30 s -- while that
/// database's primary is lost or switching: a look begun late in the bound
/// came back long after it, the pod was killed at its 30 s before its
/// lease was given up, and the standby recorded a lapse. A look still out
/// at the bound is left behind, and the member stops on time.
async fn serve_on_within<F>(
    mine: u32,
    count: u32,
    writer: bool,
    told_at_ns: i64,
    look: F,
    at_most: Duration,
    every: Duration,
) -> ServedOn
where
    F: Fn(i64, i64) -> Result<bool, String> + Send + Sync + 'static,
{
    if mine == 0 || count < 2 || !writer {
        return ServedOn::AtOnce;
    }
    let look = Arc::new(look);
    let until = tokio::time::Instant::now() + at_most;
    let mut said_why = false;
    tracing::info!(
        writer_group = mine,
        "told to stop: serving on until another writer group's writer serves"
    );
    loop {
        let asked = Arc::clone(&look);
        let now = now_ns();
        let looked = tokio::time::timeout_at(
            until,
            tokio::task::spawn_blocking(move || asked(told_at_ns, now)),
        )
        .await;
        match looked {
            Ok(Ok(Ok(true))) => {
                tracing::info!(
                    writer_group = mine,
                    "another writer group's writer serves: stopping"
                );
                return ServedOn::AnotherServes;
            }
            Ok(Ok(Ok(false))) => {}
            Ok(Ok(Err(failed))) => {
                if !said_why {
                    tracing::warn!(%failed, "the store's members could not be read; looking again");
                    said_why = true;
                }
            }
            Ok(Err(failed)) => {
                if !said_why {
                    tracing::warn!(%failed, "the look at the store's members did not run; looking again");
                    said_why = true;
                }
            }
            Err(_) => {
                tracing::warn!(
                    writer_group = mine,
                    seconds = at_most.as_secs(),
                    "the store's members were not read within the bound: stopping"
                );
                return ServedOn::Bound;
            }
        }
        if tokio::time::Instant::now() + every > until {
            tracing::info!(
                writer_group = mine,
                seconds = at_most.as_secs(),
                "no other writer group's writer was seen serving anew: stopping"
            );
            return ServedOn::Bound;
        }
        tokio::time::sleep(every).await;
    }
}

/// A writer's serve-on, told to stop, as the step-downs of its fence see
/// it (ruled 2026-11-18, item 110; scoped to a writer that is serving on).
///
/// A fenced writer stops writing at once, as ever: the epoch refuses its
/// every commit. One fenced while it serves on does not end there: until
/// its serve-on ends as it would have -- another writer group's writer
/// seen serving, or its own bound, counted from when it was told and never
/// from the fence -- it keeps its store's topics answered, each refused
/// UNAVAILABLE to be asked again ([`meridian_bus::Bus::refuse_from_here`]),
/// where the writer that took its partitions over may not serve yet and a
/// writer ended at its fence left the topics served by nobody. Then it
/// steps down as before. Fenced by its own writer group's successor -- its
/// lease, the group's, naming another member, at the fence or after it --
/// it ends at once, beside that member (the coordinator's ruling of
/// 2026-11-18 on review-c5n2 H1 and M1). A writer fenced while it is not
/// serving on -- never told to stop, the first group's, one group's only --
/// ends at once, as before: it has no serve-on, and its pod starts it
/// again.
#[derive(Clone)]
pub struct ServingOn(Arc<ServingOnSeen>);

struct ServingOnSeen {
    /// [`NOT_SERVING_ON`], [`SERVING_ON`] or [`SERVED_ON`].
    at: tokio::sync::watch::Sender<u8>,
    /// What refuses the store's topics from here, run once, at the fence.
    refuse: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

const NOT_SERVING_ON: u8 = 0;
const SERVING_ON: u8 = 1;
const SERVED_ON: u8 = 2;

impl Default for ServingOn {
    fn default() -> Self {
        Self(Arc::new(ServingOnSeen {
            at: tokio::sync::watch::channel(NOT_SERVING_ON).0,
            refuse: std::sync::Mutex::new(None),
        }))
    }
}

impl ServingOn {
    /// [`serve_on_within`] for a writer, said here from its beginning to
    /// its end; `refuse` is run once if the writer is fenced meanwhile.
    #[allow(clippy::too_many_arguments)]
    async fn within<F, R>(
        &self,
        mine: u32,
        count: u32,
        told_at_ns: i64,
        look: F,
        refuse: R,
        at_most: Duration,
        every: Duration,
    ) -> ServedOn
    where
        F: Fn(i64, i64) -> Result<bool, String> + Send + Sync + 'static,
        R: FnOnce() + Send + 'static,
    {
        // It waits on nobody ([`ServedOn::AtOnce`]): never serving on.
        if mine == 0 || count < 2 {
            return ServedOn::AtOnce;
        }
        *self.0.refuse.lock().expect("serving on poisoned") = Some(Box::new(refuse));
        self.0.at.send_replace(SERVING_ON);
        let served = serve_on_within(mine, count, true, told_at_ns, look, at_most, every).await;
        self.0.at.send_replace(SERVED_ON);
        served
    }

    /// This writer is fenced, and about to step down. Not serving on:
    /// nothing, at once. Serving on: its topics are refused from here, and
    /// this waits for the serve-on's own end -- or, where it holds a lease,
    /// until the lease names another member: its own writer group's
    /// successor, which serves its topics (the lease is its group's), and
    /// beside which it ends at once (the coordinator's ruling of 2026-11-18
    /// on review-c5n2 H1 and M1). The lease is read at once and every
    /// renew after, nothing written.
    async fn fenced<L: Leases>(
        &self,
        component: &str,
        elector: Option<&tokio::sync::Mutex<Elector<L>>>,
    ) {
        let mut at = self.0.at.subscribe();
        if *at.borrow_and_update() != SERVING_ON {
            return;
        }
        let refuse = self.0.refuse.lock().expect("serving on poisoned").take();
        if let Some(refuse) = refuse {
            tracing::warn!(
                component,
                "fenced while serving on, told to stop: writing nothing more, and answering \"ask again\" until another writer group's writer serves or the serve-on's bound"
            );
            refuse();
        }
        let Some(elector) = elector else {
            let _ = at.wait_for(|at| *at == SERVED_ON).await;
            return;
        };
        let (me, renew) = {
            let elector = elector.lock().await;
            (elector.settings().holder.clone(), elector.settings().renew)
        };
        loop {
            if let Some(successor) = successor(elector, &me, renew).await {
                tracing::info!(
                    component,
                    %successor,
                    "fenced while serving on: its own writer group's successor holds the lease; stopping at once"
                );
                return;
            }
            tokio::select! {
                served = at.wait_for(|at| *at == SERVED_ON) => {
                    let _ = served;
                    return;
                }
                _ = tokio::time::sleep(renew) => {}
            }
        }
    }
}

/// The member a writer's lease names now, where it is another than `me`:
/// its own writer group's successor, the lease being its group's. Looked
/// at alone, nothing written ([`Elector::watch`]), within `within`; none
/// where the lease names nobody or this member, or could not be read.
async fn successor<L: Leases>(
    elector: &tokio::sync::Mutex<Elector<L>>,
    me: &str,
    within: Duration,
) -> Option<String> {
    let looked = tokio::time::timeout(within, async {
        elector.lock().await.watch(Instant::now()).await
    })
    .await;
    match looked {
        Ok(Ok(Some(holder))) if holder != me => Some(holder),
        _ => None,
    }
}

/// A writer fenced, before it steps down: its lease given up where it
/// still names it (ruled 2026-11-16), then its serve-on waited out where
/// it serves on, told to stop (ruled 2026-11-18, item 110;
/// [`ServingOn`]) -- unless its own writer group's successor holds the
/// lease.
async fn fenced_stepping_down<L: Leases>(
    serving_on: &ServingOn,
    elector: Option<&tokio::sync::Mutex<Elector<L>>>,
    component: &str,
) {
    if let Some(elector) = elector {
        let_go_stepping_down(elector, component).await;
    }
    serving_on.fenced(component, elector).await;
}

/// A member's settings, from what the chart sets.
pub struct Member {
    component: &'static str,
    /// The change log's domain it follows as a standby; none for the
    /// launcher, which keeps no store.
    domain: Option<&'static str>,
    lease: Option<Settings>,
    ready_lag: Duration,
    /// A reader member (contract v30, stage B): the chart's
    /// `MERIDIAN_MEMBER_MODE=reader`.
    reader: bool,
    /// The look a standby takes before it takes its lease: whether a newer
    /// member of its group is there ([`Member::deferring_to_newer`]).
    newer: Option<Arc<NewerLook>>,
}

/// A standby's look at its store's members: whether a newer member of its
/// writer group is there now (`meridian_changes::members::Newer`).
/// Blocking: the store is read.
pub type NewerLook = dyn Fn() -> Result<bool, String> + Send + Sync;

/// The most a writer stepping down spends giving its lease up before it
/// ends (ruled 2026-11-16): past it, the lease lapses on its own.
pub const STEP_DOWN_RELEASE: Duration = Duration::from_secs(1);

/// How soon a release the API server failed is tried again, within its
/// bound: a request refused at once (the API server restarting, its
/// connection reset) is asked again, rather than leaving the lease to lapse.
pub const RELEASE_TRIED_EVERY: Duration = Duration::from_millis(200);

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
        let reader = match var("MERIDIAN_MEMBER_MODE").as_deref() {
            None | Some("") => false,
            Some("reader") => true,
            Some(other) => {
                return Err(format!(
                    "MERIDIAN_MEMBER_MODE is {other:?}; \"reader\" or unset"
                ))
            }
        };
        if reader && domain.is_none() {
            return Err(format!("{component} keeps no store, and has no readers"));
        }
        Ok(Self {
            component,
            domain,
            lease,
            ready_lag: Duration::from_millis(whole("MERIDIAN_READY_LAG_MS", READY_LAG_DEFAULT_MS)?),
            reader,
            newer: None,
        })
    }

    /// A standby that defers to a newer member of its writer group (step
    /// 3d; ruled 2026-11-16): while `look` finds one there, it takes no
    /// free lease, leaving it to the member a rollout brought at the new
    /// count, unless nobody takes it within one lease duration -- a writer
    /// at the old count is better than none.
    pub fn deferring_to_newer<F>(mut self, look: F) -> Self
    where
        F: Fn() -> Result<bool, String> + Send + Sync + 'static,
    {
        self.newer = Some(Arc::new(look));
        self
    }

    /// A member with no lease: what tests and a local stack run.
    pub fn sole(component: &'static str) -> Self {
        Self {
            component,
            domain: None,
            lease: None,
            ready_lag: Duration::from_millis(READY_LAG_DEFAULT_MS),
            reader: false,
            newer: None,
        }
    }

    /// A reader member of `domain`'s store with `ready_lag` its bound: what
    /// tests run.
    pub fn reader_of(component: &'static str, domain: &'static str, ready_lag: Duration) -> Self {
        Self {
            component,
            domain: Some(domain),
            lease: None,
            ready_lag,
            reader: true,
            newer: None,
        }
    }

    /// Whether this member is a reader, serving its store's reads alone.
    pub fn is_reader(&self) -> bool {
        self.reader
    }

    /// Serve as a reader (contract v30, stage B): its reads held to their
    /// `read_after` by what it has applied of the change log, `followed`,
    /// from where it starts -- the primary's heads as it started, before its
    /// hot layer held anything ([`Followed::from_heads`]) -- and answered
    /// from the primary, at `primary`, past their wait. Following, each
    /// change applied to `apply` in order, then `followed` moved; ready while
    /// its lag is under its bound. Called once the store's reads are
    /// registered, the bus serving its reads alone.
    pub fn read(
        &self,
        bus: &Arc<Bus>,
        followed: Arc<Followed>,
        apply: Arc<dyn Apply>,
        primary: Arc<dyn meridian_bus::read_after::Position>,
    ) -> Result<(), String> {
        let domain = self
            .domain
            .ok_or_else(|| format!("{} keeps no store to read", self.component))?;
        said().set_mode(Mode::Reader);
        said().set_partitions(followed.heads().into_keys().collect());
        let reads = bus.reads_after();
        reads.set_bound(self.ready_lag);
        reads.keep(domain, Arc::clone(&followed) as Arc<_>);
        reads.keep_primary(domain, Arc::clone(&primary));
        tokio::spawn(follow(
            Arc::clone(bus),
            domain,
            self.ready_lag,
            Some(Following {
                followed: Arc::clone(&followed),
                apply,
                primary,
            }),
        ));
        tracing::info!(
            component = self.component,
            member = %member_name(),
            "a reader: serving reads, following the writer's change log"
        );
        Ok(())
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
        // Never told: the sender is kept for as long as the wait.
        let (_never, mut told) = tokio::sync::watch::channel(false);
        match self.wait_to_write_unless(bus, &mut told).await? {
            Some(holding) => Ok(holding),
            None => Err(format!(
                "{} stopped waiting to write, told by nobody",
                self.component
            )),
        }
    }

    /// [`Member::wait_to_write`], unless told to stop meanwhile: `None`
    /// then, this member holding nothing. A standby told to stop stops at
    /// once; a stop that lands while it takes the lease lets the take
    /// finish and gives the lease up (ruled 2026-11-15): it never ends
    /// holding one.
    pub async fn wait_to_write_unless(
        &self,
        bus: &Arc<Bus>,
        told: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<Option<Holding>, String> {
        // A read naming a `read_after` this member has not reached waits up
        // to its bound, `<store>.readyLagMs`, and is never answered from
        // behind it (contract v30, stage B; spec section 5).
        bus.reads_after().set_bound(self.ready_lag);
        // Who raises an epoch, where its store raises one itself on a new
        // database timeline (contract v30, stage C).
        meridian_changes::set_member(&member_name());
        let Some(settings) = self.lease.clone() else {
            said().set_mode(Mode::Writer);
            return Ok(Some(Holding {
                elector: None,
                component: self.component,
                why: "",
                previous: String::new(),
                serving_on: ServingOn::default(),
            }));
        };
        said().set_mode(Mode::Standby);
        tokio::spawn(report_forever(Arc::clone(bus), self.component));
        let api = KubeLeases::in_cluster(settings.renew)?;
        let look_every = settings.look_every();
        let mut elector = store_elector(api, settings);
        let following = self
            .domain
            .map(|domain| tokio::spawn(follow(Arc::clone(bus), domain, self.ready_lag, None)));
        if following.is_none() {
            // Nothing to follow: a standby of the launcher is ready while it
            // waits.
            crate::mark_ready(true);
        }
        let holds = stand_by(
            &mut elector,
            look_every,
            self.component,
            told,
            self.newer.clone(),
        )
        .await;
        // Stopped, and waited for: a follower aborted mid-poll on another
        // thread still finished that poll, and its readiness written then
        // -- mode "standby", maybe not ready -- landed after the writer's
        // own (a local run: the street's writer served, said "standby"
        // and the takeover's member set never settled).
        if let Some(following) = following {
            following.abort();
            let _ = following.await;
        }
        if !holds {
            return Ok(None);
        }
        crate::mark_ready(false);
        said().set_lag_ms(0);
        said().set_mode(Mode::Writer);
        tracing::info!(
            component = self.component,
            member = %member_name(),
            "this member holds the lease: taking the writer's partitions over"
        );
        let (why, from) = elector.takeover();
        Ok(Some(Holding {
            elector: Some(Arc::new(tokio::sync::Mutex::new(elector))),
            component: self.component,
            why,
            previous: if from.is_empty() {
                from
            } else {
                holder_member(&from)
            },
            serving_on: ServingOn::default(),
        }))
    }
}

/// A store member's elector: a record held seen, and a lapse judged, by
/// when its read answered (review c5a10 M1; PO item 54): a read slow across
/// the writer's renewal never lets its standby take inside the Lease. A
/// look that failed still drops what was seen (CI 42958621081).
pub(crate) fn store_elector<L: Leases>(api: L, settings: Settings) -> Elector<L> {
    Elector::new(api, settings).seen_as_answered(Instant::now)
}

/// Stand by on a lease until this member holds it, or is told to stop:
/// whether it holds it.
///
/// A look that is out is always finished, never dropped where it stands:
/// the write that takes the lease may have landed before its answer is
/// read, and a member that stopped there would end holding a lease nobody
/// renews, its store unwritten until the lapse (step 3d, ruled 2026-11-15).
/// Told to stop, it is heard between looks; where the look just finished
/// took the lease, the lease is given up before this returns.
pub(crate) async fn stand_by<L: Leases>(
    elector: &mut Elector<L>,
    look_every: Duration,
    component: &'static str,
    told: &mut tokio::sync::watch::Receiver<bool>,
    newer: Option<Arc<NewerLook>>,
) -> bool {
    let mut said_waiting = false;
    let mut said_deferring = false;
    // When a newer member of its group was last found there, and since
    // when the lease has been free while this member deferred, each by its
    // own clock (ruled 2026-11-16).
    let mut newer_seen: Option<tokio::time::Instant> = None;
    let mut free_since: Option<tokio::time::Instant> = None;
    let duration = elector.settings().duration;
    // A look that failed may have taken the lease: its write landed and
    // its answer was lost (the review's L2).
    let mut may_hold = false;
    loop {
        if *told.borrow() {
            if may_hold {
                give_up_if_mine(elector, component).await;
            }
            return false;
        }
        if let Some(look) = &newer {
            let look = Arc::clone(look);
            // A look that fails or does not come back defers nothing:
            // better a writer at the old count than none.
            if let Ok(Ok(Ok(true))) =
                tokio::time::timeout(look_every, tokio::task::spawn_blocking(move || look())).await
            {
                newer_seen = Some(tokio::time::Instant::now());
            }
        }
        let deferring = newer_seen.is_some_and(|at| at.elapsed() < duration);
        if !deferring {
            free_since = None;
        } else {
            // Watched, never taken, while the newer member may take it.
            match elector.watch(Instant::now()).await {
                Ok(Some(_)) => free_since = None,
                Ok(None) => {
                    let since = *free_since.get_or_insert_with(tokio::time::Instant::now);
                    if !said_deferring {
                        tracing::info!(
                            component,
                            "a standby: a newer member of its group is there; the free lease is left to it"
                        );
                        said_deferring = true;
                    }
                    if since.elapsed() >= duration {
                        tracing::warn!(
                            component,
                            "a standby: no newer member took the free lease within its duration; taking it at this member's count"
                        );
                        newer_seen = None;
                    }
                }
                Err(_) => {}
            }
            if newer_seen.is_some() {
                tokio::select! {
                    _ = tokio::time::sleep(look_every) => {}
                    heard = told.changed() => {
                        if heard.is_err() {
                            tokio::time::sleep(look_every).await;
                        }
                    }
                }
                continue;
            }
        }
        match elector.look(Instant::now(), now_ns()).await {
            Ok(Standing::Held) => {
                if *told.borrow() {
                    tracing::info!(component, "told to stop as it took the lease: giving it up");
                    give_up_if_mine(elector, component).await;
                    return false;
                }
                return true;
            }
            Ok(Standing::HeldBy(holder)) => {
                if !said_waiting {
                    tracing::info!(
                        component,
                        writer = %holder,
                        "a standby: following the writer until its lease lapses"
                    );
                    said_waiting = true;
                }
            }
            Err(failed) => {
                may_hold = true;
                tracing::warn!(component, %failed, "the lease could not be read; looking again");
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(look_every) => {}
            heard = told.changed() => {
                // Nobody left to tell it: it waits its look out as before.
                if heard.is_err() {
                    tokio::time::sleep(look_every).await;
                }
            }
        }
    }
}

/// A writer's hold on its lease, from taking it to stopping.
pub struct Holding {
    elector: Option<Arc<tokio::sync::Mutex<Elector<KubeLeases>>>>,
    component: &'static str,
    why: &'static str,
    /// The member that held the lease before, where one did.
    previous: String,
    /// Its serve-on once it is told to stop, which a fence waits out.
    serving_on: ServingOn,
}

/// The member a lease's holder names: its pod's name, before the process's
/// mark (`<pod>_<hex>`; pod names never hold an underscore).
pub fn holder_member(holder: &str) -> String {
    holder
        .rsplit_once('_')
        .map_or(holder, |(pod, _)| pod)
        .to_string()
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
            previous: self.previous.clone(),
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

    /// Follow the database's timeline while writing (contract v30, stage C;
    /// the resilience spec's section 3). The store raises its epoch itself
    /// before it commits on a new one -- each connection its pool opens reads
    /// the database's identity -- and `look`, the store's own look, is taken
    /// at once, for a restore or a restart on a new timeline, and every
    /// [`TIMELINE_LOOK_EVERY`] after, off the runtime's threads. Once a store
    /// of this process raised its epoch so, what this process committed of
    /// `domain` is forgotten by its reads held to their `read_after`: the
    /// record may have lost some of it. Where the record's epoch is
    /// another's, this member steps down.
    pub fn follow_timeline<F>(&self, bus: &Arc<Bus>, domain: &'static str, look: F)
    where
        F: Fn() -> Result<meridian_changes::timeline::Timeline, String> + Send + Sync + 'static,
    {
        use meridian_changes::timeline::Timeline;
        let component = self.component;
        let bus = Arc::clone(bus);
        let look = Arc::new(look);
        let elector = self.elector.clone();
        let serving_on = self.serving_on.clone();
        tokio::spawn(async move {
            let mut raised = meridian_changes::timeline::raised();
            let mut said_failing = false;
            loop {
                let looking = Arc::clone(&look);
                match one_look_at_a_time(move || looking()).await {
                    Ok(Ok(Timeline::NotMine(epoch))) => {
                        fenced_stepping_down(&serving_on, elector.as_deref(), component).await;
                        step_down(
                            component,
                            &format!("the store's epoch is {epoch}, another writer's"),
                        )
                    }
                    Ok(Ok(Timeline::First)) => tracing::info!(
                        component,
                        "the database's identity is kept: a new timeline raises the epoch"
                    ),
                    Ok(Ok(_)) => said_failing = false,
                    Ok(Err(failed)) => {
                        if !said_failing {
                            tracing::warn!(component, %failed, "the database's timeline could not be read; looking again");
                            said_failing = true;
                        }
                    }
                    Err(failed) => {
                        tracing::warn!(component, %failed, "the timeline's look failed")
                    }
                }
                let now = meridian_changes::timeline::raised();
                if now != raised {
                    raised = now;
                    bus.reads_after().forget(domain);
                    tracing::warn!(
                        component,
                        "the store raised its epoch on a new database timeline: what this member committed is read again from the record"
                    );
                }
                tokio::time::sleep(TIMELINE_LOOK_EVERY).await;
            }
        });
    }

    /// Keep the lease while writing: renewed every renew, each renewal
    /// giving up within it; the member steps down when the lease shows
    /// another holder, when `epoch_now` -- the store's head epoch, read from
    /// the record -- moves past `mine` -- the store's own, which a new
    /// database timeline raises (contract v30, stage C) -- or when a commit
    /// of its own was fenced. A renewal that fails alone never moves it
    /// (contract v30, stage C: the API server lost). Where no lease is held,
    /// the epoch alone is watched: passed, or a commit fenced, the writer
    /// steps down. A writer fenced while it serves on, told to stop, steps
    /// down when its serve-on ends, its topics refused meanwhile, to be
    /// asked again (ruled 2026-11-18, item 110; [`ServingOn`]).
    ///
    /// The epoch is read on a loop of its own: a read waits on the store's
    /// pool, up to its connection timeout while the database's primary is
    /// replaced, and a renewal waiting behind it let the lease lapse under a
    /// writer that was serving (CI 42291353103: the book's writer renewed
    /// nothing for 15 s of a database switchover, and its standby took over).
    pub fn keep<M, E>(&self, mine: M, epoch_now: E)
    where
        M: Fn() -> u64 + Send + Sync + 'static,
        E: Fn() -> Result<u64, String> + Send + Sync + 'static,
    {
        self.keep_or(mine, epoch_now, |component, why| step_down(component, why));
    }

    /// [`Self::keep`], where a writer with no lease steps down with `down`:
    /// [`step_down`], or a test's own.
    fn keep_or<M, E, D>(&self, mine: M, epoch_now: E, down: D)
    where
        M: Fn() -> u64 + Send + Sync + 'static,
        E: Fn() -> Result<u64, String> + Send + Sync + 'static,
        D: Fn(&'static str, &str) + Send + Sync + 'static,
    {
        let component = self.component;
        let epoch_now = Arc::new(epoch_now);
        let serving_on = self.serving_on.clone();
        let Some(elector) = self.elector.clone() else {
            // No lease -- tolerant's, one member a group -- and the epoch
            // watched all the same (spec section 3, "a frozen writer"): a
            // writer whose epoch another raised steps down, and its pod
            // starts it again under a new one (CI 43804841979, reconfigure,
            // groups raised in tolerant: group 0's writer raised every
            // group's epoch on the database's new timeline, and group 1's
            // writer, fenced, refused every write for five minutes, ready).
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(UNLEASED_EPOCH_LOOK_EVERY).await;
                    if meridian_changes::fenced_seen() {
                        fenced_stepping_down::<KubeLeases>(&serving_on, None, component).await;
                        return down(
                            component,
                            "a commit of this writer was refused by a newer epoch",
                        );
                    }
                    let reading = Arc::clone(&epoch_now);
                    match one_look_at_a_time(move || reading()).await {
                        Ok(Ok(head)) if head > mine() => {
                            fenced_stepping_down::<KubeLeases>(&serving_on, None, component).await;
                            return down(
                                component,
                                &format!(
                                    "the store's epoch is {head}, past this writer's {}",
                                    mine()
                                ),
                            );
                        }
                        Ok(Ok(_)) => {}
                        Ok(Err(failed)) => {
                            tracing::warn!(component, %failed, "the store's epoch could not be read")
                        }
                        Err(failed) => tracing::warn!(component, %failed, "the epoch check failed"),
                    }
                }
            });
            return;
        };
        let down = Arc::new(down);
        let downing = Arc::clone(&down);
        tokio::spawn(watch_the_epoch(
            Arc::clone(&elector),
            component,
            serving_on.clone(),
            mine,
            epoch_now,
            move |component, why| downing(component, why),
        ));
        tokio::spawn(renew_while_writing(
            elector,
            component,
            serving_on,
            move |component, why| down(component, why),
        ));
    }

    /// What a task of this writer's own steps down with, giving the lease
    /// up first as the epoch's and the fence's step-downs do (ruled
    /// 2026-11-16): a writer that ended where it stood left its standby
    /// to wait out the lease (CI 43799885541, ha groups raised: the lake's
    /// old writer, its write-ahead commit fenced by the added group's
    /// takeover, ended holding its lease, which lapsed).
    pub fn stepping_down(&self) -> SteppingDown {
        SteppingDown {
            elector: self.elector.clone(),
            component: self.component,
            serving_on: self.serving_on.clone(),
        }
    }

    /// [`serve_on_until_another_group_serves`] for this writer, told to
    /// stop at `told_at_ns`, and known to its fence's step-downs (ruled
    /// 2026-11-18, item 110; [`ServingOn`]): fenced meanwhile, it is not
    /// ready, `refuse` is run -- its store's topics refused from here, to
    /// be asked again -- and it ends when this does, not at the fence.
    pub async fn serve_on_until_another_group_serves<F, R>(
        &self,
        mine: u32,
        count: u32,
        told_at_ns: i64,
        look: F,
        refuse: R,
    ) -> ServedOn
    where
        F: Fn(i64, i64) -> Result<bool, String> + Send + Sync + 'static,
        R: FnOnce() + Send + 'static,
    {
        self.serving_on
            .within(
                mine,
                count,
                told_at_ns,
                look,
                move || {
                    crate::mark_ready(false);
                    refuse()
                },
                SERVES_ON_AT_MOST,
                SERVES_ON_LOOKS_EVERY,
            )
            .await
    }

    /// Give the lease up on a planned stop: the standby takes over at its
    /// next look rather than after the lease lapses ([`release_on_stop`]).
    pub async fn release(&self) {
        if let Some(elector) = &self.elector {
            release_on_stop(elector, self.component).await;
        }
    }
}

/// A writer's lease given up on a planned stop ([`Holding::release`]): read
/// again and released where it still names this member, by a write
/// conditional on the version read, a request that failed tried again
/// every [`RELEASE_TRIED_EVERY`] for as long as the lease lasts unrenewed --
/// past that it has lapsed on its own. Never on what its last renewal left
/// seen: a renewal that failed left nothing, and one whose answer was lost
/// left a version gone, and either stop left the lease to lapse, said
/// nowhere (reconfigure, "ha databases added", c5c-ci1's local run on
/// 9479a69c: the book's group-1 writer, stopped at 01:31:39, still named on
/// its lease at 01:32:04; its standby took it as lapsed at 01:32:11).
pub(crate) async fn release_on_stop<L: Leases>(
    elector: &tokio::sync::Mutex<Elector<L>>,
    component: &str,
) {
    // Bounded from the stop, set before the elector is taken, as a
    // step-down's bound is (review c5i L5): a renewal in flight holds the
    // elector up to a renew, and a bound begun after that wait ended a
    // renew past the lease's duration, and past the termination grace.
    let stopped = tokio::time::Instant::now();
    let mut elector = elector.lock().await;
    let until = stopped + elector.settings().duration;
    let released = tokio::time::timeout_at(
        until,
        elector.release_if_mine_by(now_ns(), until, RELEASE_TRIED_EVERY),
    )
    .await;
    match released {
        Ok(Ok(true)) => tracing::info!(component, "stopping: the lease is released"),
        Ok(Ok(false)) => {}
        Ok(Err(failed)) => {
            tracing::warn!(component, %failed, "stopping: the lease could not be released; it lapses on its own")
        }
        Err(_) => tracing::warn!(
            component,
            "stopping: the lease was not released within its duration; it lapses on its own"
        ),
    }
}

/// A leased writer's look at its store's head epoch, every renew, on a
/// loop of its own ([`Holding::keep`]'s other half): passed, the writer is
/// fenced, and steps down with `down` ([`fenced_stepping_down`]).
async fn watch_the_epoch<L, M, E, D>(
    elector: Arc<tokio::sync::Mutex<Elector<L>>>,
    component: &'static str,
    serving_on: ServingOn,
    mine: M,
    epoch_now: Arc<E>,
    down: D,
) where
    L: Leases + 'static,
    M: Fn() -> u64 + Send + Sync + 'static,
    E: Fn() -> Result<u64, String> + Send + Sync + 'static,
    D: Fn(&'static str, &str) + Send + Sync + 'static,
{
    let renew = elector.lock().await.settings().renew;
    loop {
        tokio::time::sleep(renew).await;
        let reading = Arc::clone(&epoch_now);
        match tokio::task::spawn_blocking(move || reading()).await {
            Ok(Ok(head)) if head > mine() => {
                fenced_stepping_down(&serving_on, Some(&elector), component).await;
                return down(
                    component,
                    &format!("the store's epoch is {head}, past this writer's {}", mine()),
                );
            }
            Ok(Ok(_)) => {}
            Ok(Err(failed)) => {
                tracing::warn!(component, %failed, "the store's epoch could not be read")
            }
            Err(failed) => {
                tracing::warn!(component, %failed, "the epoch check failed")
            }
        }
    }
}

/// A writer's renewals of the lease it holds, from its take until it steps
/// down with `down`: [`Holding::keep`]'s half that keeps the lease.
///
/// A renewal is begun every renew from when the one before it began, and
/// each gives up within one renew: one the API server never answered is
/// followed by the next at once, so two are tried within the renew and two
/// more renews after the last that landed -- the spec's Patroni rule
/// (`loop_wait + 2 x retry_timeout <= ttl`, section 2), which the chart
/// holds the lease's duration to. Begun a renew after the one that gave
/// up, the next landed a whole lease after the last, and a standby looking
/// took the lease as lapsed under a writer serving (c5c-ci1's local
/// reconfigure on 9479a69c: the conductor's writer, a renewal unanswered at
/// 01:32:03, its next refused at 01:32:06 by its standby's take, recorded
/// "the writer's lease lapsed").
async fn renew_while_writing<L, D>(
    elector: Arc<tokio::sync::Mutex<Elector<L>>>,
    component: &'static str,
    serving_on: ServingOn,
    down: D,
) where
    L: Leases + 'static,
    D: Fn(&'static str, &str) + Send + Sync + 'static,
{
    let (renew, me) = {
        let elector = elector.lock().await;
        (elector.settings().renew, elector.settings().holder.clone())
    };
    // The API server lost (contract v30, stage C; the resilience spec's
    // section 2, approved 2026-11-10): a renewal that fails -- the API
    // server not answering -- never moves this writer. It keeps writing,
    // and stays ready, while its commits pass the epoch, as Patroni's
    // failsafe_mode keeps a primary through its store's outage: nobody can
    // take a lease the API server cannot grant, and a takeover that did
    // would raise the epoch, which refuses this writer's next commit (and
    // the epoch's own loop finds). Each renewal gives up within one renew.
    let mut unrenewed_since: Option<Instant> = None;
    let mut next = tokio::time::Instant::now() + renew;
    loop {
        tokio::time::sleep_until(next).await;
        next = tokio::time::Instant::now() + renew;
        if meridian_changes::fenced_seen() {
            fenced_stepping_down(&serving_on, Some(&elector), component).await;
            return down(
                component,
                "a commit of this writer was refused by a newer epoch",
            );
        }
        // Given up on a planned stop: not looked at again, so a member
        // still stopping is never told another holds it, and steps down
        // only once a commit of its own is refused.
        if elector.lock().await.gave_up() {
            continue;
        }
        let looked = tokio::time::timeout(renew, async {
            elector.lock().await.look(Instant::now(), now_ns()).await
        })
        .await
        .unwrap_or_else(|_| {
            Err(format!(
                "the API server did not answer within {} s",
                renew.as_secs()
            ))
        });
        match looked {
            Ok(Standing::Held) => {
                if let Some(since) = unrenewed_since.take() {
                    tracing::info!(
                        component,
                        unrenewed_s = since.elapsed().as_secs(),
                        "the lease is renewed again; this writer wrote on under its epoch meanwhile"
                    );
                }
            }
            // Its own writer group's successor, the lease being its group's:
            // it ends at once, serving on or not (the coordinator's ruling
            // of 2026-11-18 on review-c5n2 H1), the successor serving.
            Ok(Standing::HeldBy(holder)) if holder != me => {
                return down(
                    component,
                    &format!("another member ({holder}) holds the lease"),
                );
            }
            // A write came between this renewal's read, which named this
            // writer, and its own, and the lease read again within the
            // renewal named this writer or nobody, or was not answered
            // (review-c5i L4): its last renewal, cut at its bound and landing
            // late. Unrenewed, and read again at the next renewal.
            Ok(Standing::HeldBy(_)) => {
                if unrenewed_since.is_none() {
                    tracing::warn!(
                        component,
                        "the lease could not be renewed: written meanwhile; read again at the next renewal"
                    );
                }
                unrenewed_since.get_or_insert_with(Instant::now);
            }
            Err(failed) => {
                if unrenewed_since.is_none() {
                    tracing::warn!(
                        component,
                        %failed,
                        "the lease could not be renewed: writing on under the epoch, which refuses this writer's commits if another takes over"
                    );
                }
                unrenewed_since.get_or_insert_with(Instant::now);
            }
        }
    }
}

/// A member stopping gives up a lease that names it, within
/// [`STEP_DOWN_RELEASE`]: read again, and released only where it still
/// names this member (`release_if_mine`).
async fn give_up_if_mine<L: Leases>(elector: &mut Elector<L>, component: &str) {
    let until = tokio::time::Instant::now() + STEP_DOWN_RELEASE;
    match tokio::time::timeout_at(
        until,
        elector.release_if_mine_by(now_ns(), until, RELEASE_TRIED_EVERY),
    )
    .await
    {
        Ok(Ok(true)) => tracing::info!(component, "stopping: the lease given up"),
        Ok(Ok(false)) => {}
        Ok(Err(failed)) => {
            tracing::warn!(component, %failed, "stopping: the lease could not be given up; it lapses on its own")
        }
        Err(_) => tracing::warn!(
            component,
            "stopping: the lease was not given up within its bound; it lapses on its own"
        ),
    }
}

/// A writer stepping down gives its lease up first, within
/// [`STEP_DOWN_RELEASE`] (ruled 2026-11-16): its standby takes over at its
/// next look rather than after the lease lapses -- a write gap of up to the
/// lease's duration (CI 43788516279: group 0's lake writer, its epoch
/// passed, stepped down holding its lease, and its standby waited it out).
/// Only a lease still this member's is released (`release_if_mine`).
pub async fn let_go_stepping_down<L: Leases>(
    elector: &tokio::sync::Mutex<Elector<L>>,
    component: &str,
) {
    let until = tokio::time::Instant::now() + STEP_DOWN_RELEASE;
    let released = tokio::time::timeout_at(until, async {
        elector
            .lock()
            .await
            .release_if_mine_by(now_ns(), until, RELEASE_TRIED_EVERY)
            .await
    })
    .await;
    match released {
        Ok(Ok(true)) => tracing::info!(component, "stepping down: the lease given up"),
        Ok(Ok(false)) => {}
        Ok(Err(failed)) => {
            tracing::warn!(component, %failed, "stepping down: the lease could not be given up; it lapses on its own")
        }
        Err(_) => tracing::warn!(
            component,
            "stepping down: the lease was not given up within its bound; it lapses on its own"
        ),
    }
}

/// A writer's step-down for a task of its own ([`Holding::stepping_down`]).
pub struct SteppingDown {
    elector: Option<Arc<tokio::sync::Mutex<Elector<KubeLeases>>>>,
    component: &'static str,
    serving_on: ServingOn,
}

impl SteppingDown {
    /// The lease given up where it still names this member (within
    /// [`STEP_DOWN_RELEASE`]), then [`step_down`].
    pub async fn now(self, why: &str) -> ! {
        fenced_stepping_down(&self.serving_on, self.elector.as_deref(), self.component).await;
        step_down(self.component, why)
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
    // Waiting on what it needs to do its whole job: degraded, saying why.
    let waiting = said().waiting();
    let state = if waiting.is_empty() {
        meridian_domain::v1::ComponentHealth::Serving
    } else {
        meridian_domain::v1::ComponentHealth::Degraded
    };
    meridian_domain::v1::ComponentReport {
        component: component.to_string(),
        version: crate::VERSION.to_string(),
        schema_version: schema,
        health: state as i32,
        detail: waiting,
        started_at_ns,
        mode: said().mode(),
        zone: zone(),
        lag_ms: said().lag_ms(),
        partitions: said().partitions(),
        member: member_name(),
        // The busiest share of a minute its writer group's commit lane was
        // busy, over the profile's window (step 3d; approved 2026-11-14):
        // a writer's alone.
        peak_write_busy_percent: if said().mode() == "writer" {
            meridian_changes::busy::peak_percent(meridian_changes::now_ns())
        } else {
            0
        },
        ..health()
    }
}

/// Its memory, what its budget did and its restarts (stage M; the resilience
/// spec's section 10a; spec/a-deployment-is-observable-by-open-standards,
/// ruled item 2), from the one place the component keeps them: its budget's
/// accounting, and the restarts its pod's own scratch has counted.
pub fn health() -> meridian_domain::v1::ComponentReport {
    let said = meridian_budget::installed().map(meridian_budget::Process::accounting);
    let said = said.unwrap_or_default();
    let mut refusals = Vec::new();
    if said.refused > 0 {
        refusals.push(meridian_domain::v1::RefusalCount {
            reason: meridian_pb::v1::RefusalReason::Overloaded
                .as_str_name()
                .to_string(),
            count: said.refused,
        });
    }
    let (restarts, last_restart_at_ns) = restarts_counted();
    meridian_domain::v1::ComponentReport {
        memory_bytes: said.resident.unwrap_or(0),
        memory_limit_bytes: said.limit.unwrap_or(0),
        memory_budget_bytes: said.working.unwrap_or(0),
        memory_peak_bytes: said.peak_resident,
        reserved_bytes: said.reserved,
        refusals,
        sheds: said.shed,
        restarts,
        last_restart_reason: String::new(),
        last_restart_at_ns,
        ..Default::default()
    }
}

/// Where a container's starts are counted: beside its ready file, in its
/// pod's own scratch, which outlives a container's restart and not its pod.
fn starts_file() -> Option<std::path::PathBuf> {
    let ready = crate::var("MERIDIAN_READY_FILE")?;
    Some(std::path::Path::new(&ready).parent()?.join("starts"))
}

static STARTS: std::sync::OnceLock<(u32, i64)> = std::sync::OnceLock::new();

/// Count this start (stage M): the starts before it in this pod are its
/// restarts, and the last of them when it restarted. Once a process; a
/// component with no ready file counts none.
pub fn count_this_start(now_ns: i64) -> (u32, i64) {
    *STARTS.get_or_init(|| match starts_file() {
        Some(path) => counted_start(&path, now_ns),
        None => (0, 0),
    })
}

/// This start counted in `path`: the starts before it, and when this one
/// was, where there were any.
fn counted_start(path: &std::path::Path, now_ns: i64) -> (u32, i64) {
    let before = std::fs::read_to_string(path).unwrap_or_default();
    let starts: u32 = before
        .split_whitespace()
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    let _ = std::fs::write(path, format!("{} {now_ns}\n", starts + 1));
    if starts == 0 {
        (0, 0)
    } else {
        (starts, now_ns)
    }
}

#[cfg(test)]
mod health_tests {
    use super::*;

    #[test]
    fn a_containers_starts_are_counted_in_its_pods_scratch_and_the_first_is_no_restart() {
        let dir = std::env::temp_dir().join(format!("starts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("starts");
        let _ = std::fs::remove_file(&path);
        assert_eq!(counted_start(&path, 10), (0, 0), "the first start");
        assert_eq!(counted_start(&path, 20), (1, 20), "restarted once, at 20");
        assert_eq!(counted_start(&path, 30), (2, 30));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_clusters_word_names_a_members_restarts_and_the_last_ones_reason() {
        let pods = serde_json::json!({"items": [
            {"metadata": {"name": "meridian-lake-0"}, "status": {"containerStatuses": [
                {"restartCount": 2, "lastState": {"terminated": {"reason": "Error", "finishedAt": "2026-11-09T10:00:00Z"}}},
                {"restartCount": 1, "lastState": {"terminated": {"reason": "OOMKilled", "finishedAt": "2026-11-09T11:00:00Z"}}}
            ]}},
            {"metadata": {"name": "meridian-bor-0"}, "status": {"containerStatuses": [{"restartCount": 0, "lastState": {}}]}}
        ]});
        let said = crate::restarts_of(&pods);
        assert_eq!(said["meridian-lake-0"], (3, "OOMKilled".to_string()));
        assert_eq!(said["meridian-bor-0"], (0, String::new()));
        let report = meridian_domain::v1::ComponentReport {
            member: "meridian-lake-0".into(),
            restarts: 1,
            ..Default::default()
        };
        let told = crate::with_restarts(report, &said);
        assert_eq!(
            (told.restarts, told.last_restart_reason.as_str()),
            (3, "OOMKilled")
        );
    }

    #[test]
    fn the_report_carries_the_budgets_accounting_and_defaults_with_none_installed() {
        let said = health();
        // No budget installed in a test: nothing reserved, refused or shed.
        assert_eq!((said.reserved_bytes, said.sheds), (0, 0));
        assert!(said.refusals.is_empty());
    }
}

fn restarts_counted() -> (u32, i64) {
    STARTS.get().copied().unwrap_or((0, 0))
}

/// What a reader member applies each change it follows to: its store's hot
/// layer (contract v30, stage B).
pub trait Apply: Send + Sync {
    /// `changes` of `partition`, in order, each following the last applied.
    fn apply(&self, partition: &str, changes: &[meridian_changes::Change]);

    /// Everything held of `partition` dropped: changes of it were not
    /// followed, so nothing held of it is known to be current.
    fn reset(&self, partition: &str);
}

/// A reader member's position: each partition's head as far as it has
/// applied the change log, what its reads are held to.
pub struct Followed {
    at: Mutex<HashMap<String, u64>>,
}

impl Followed {
    /// A reader's position as it starts: the primary's heads, read before
    /// its hot layer holds anything. Made before its store, which stamps
    /// each answer from its hot layer with it (spec section 5: a reader's
    /// reply carries its own applied position).
    pub fn from_heads(start: impl IntoIterator<Item = (String, u64)>) -> Arc<Self> {
        Arc::new(Self {
            at: Mutex::new(start.into_iter().collect()),
        })
    }

    fn set(&self, partition: &str, head: u64) {
        self.at
            .lock()
            .expect("followed poisoned")
            .insert(partition.to_string(), head);
    }

    /// Each partition's head as applied.
    pub fn heads(&self) -> HashMap<String, u64> {
        self.at.lock().expect("followed poisoned").clone()
    }
}

impl meridian_bus::read_after::Position for Followed {
    fn heads(&self, partitions: &[String]) -> Result<HashMap<String, u64>, String> {
        let at = self.at.lock().expect("followed poisoned");
        Ok(partitions
            .iter()
            .filter_map(|partition| at.get(partition).map(|head| (partition.clone(), *head)))
            .collect())
    }
}

/// A store's position read from its change log, partition by partition: a
/// read replica's, or a primary's whose store reads no heads whole.
pub struct LogHeads<L: ?Sized> {
    log: Arc<L>,
    /// Every partition its log keeps, where it lists them: what a reader
    /// reconciles with as its writer answers it ([`Position::all_heads`]).
    ///
    /// [`Position::all_heads`]: meridian_bus::read_after::Position::all_heads
    every: Vec<String>,
}

impl<L: ?Sized> LogHeads<L> {
    /// Read partition by partition, listing none: a read replica's position.
    pub fn of(log: Arc<L>) -> Self {
        Self {
            log,
            every: Vec::new(),
        }
    }

    /// Read partition by partition, `every` partition its log keeps listed:
    /// a primary a reader reconciles with (the instrument store's, whose
    /// log keeps its one partition and `control`).
    pub fn listing(log: Arc<L>, every: &[&str]) -> Self {
        Self {
            log,
            every: every
                .iter()
                .map(|partition| partition.to_string())
                .collect(),
        }
    }
}

impl<L> meridian_bus::read_after::Position for LogHeads<L>
where
    L: meridian_changes::ChangeLog + ?Sized,
{
    fn heads(&self, partitions: &[String]) -> Result<HashMap<String, u64>, String> {
        let mut heads = HashMap::new();
        for partition in partitions {
            // From past the head: the head alone, no change read.
            match self.log.read_changes(partition, i64::MAX as u64, 1) {
                Ok(reply) => {
                    heads.insert(partition.clone(), reply.head);
                }
                Err(meridian_changes::ReadError::UnknownPartition(_)) => {}
                Err(failed) => return Err(failed.to_string()),
            }
        }
        Ok(heads)
    }

    fn all_heads(&self) -> Option<Result<HashMap<String, u64>, String>> {
        (!self.every.is_empty()).then(|| self.heads(&self.every))
    }
}

/// A reader's following: where it has applied to, what applies, and its
/// store's primary, which it reconciles with as its writer answers it.
struct Following {
    followed: Arc<Followed>,
    apply: Arc<dyn Apply>,
    primary: Arc<dyn meridian_bus::read_after::Position>,
}

/// How often a writer looks at its database's timeline beside what its
/// pool's connections see (contract v30, stage C).
pub const TIMELINE_LOOK_EVERY: Duration = Duration::from_secs(5);

/// How often a writer with no lease reads its store's head epoch: as often
/// as a leased one, at `leases.renewSeconds`' default.
pub const UNLEASED_EPOCH_LOOK_EVERY: Duration = Duration::from_secs(2);

/// A member's periodic look at its store -- its row among the members every
/// 2 s, its head epoch every 2 s with no lease, its database's timeline every
/// 5 s -- made off the runtime's threads one at a time. Made at once, the
/// looks due together each took a thread of the blocking pool, which kept
/// two or three of them, and their memory, resident in every writer
/// (c30d-t7: the cluster's footprint median at its bound). Answers what
/// `read` answered.
async fn one_look_at_a_time<T: Send + 'static>(
    read: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    static LOOKING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _alone = LOOKING.lock().await;
    tokio::task::spawn_blocking(read).await
}

/// How often a standby asks its writer, which is also its interest in the
/// tail (ruling 1062).
const ASK_EVERY: Duration = Duration::from_secs(1);

/// A standby following its writer's change log: each page heard on the
/// tail applied where it follows the last, a page missed read from the
/// writer, and the writer asked every second -- for its liveness, and so it
/// keeps publishing the tail. Ready while its lag is under `bound`.
///
/// A reader's following applies each change too, from the heads it started
/// at: a page that follows on, a gap read from the writer, and a partition
/// first heard (or past retention) reset and held from its head.
async fn follow(
    bus: Arc<Bus>,
    domain: &'static str,
    bound: Duration,
    following: Option<Following>,
) {
    let mut tail = bus.subscribe_every(&meridian_changes::every_changes_topic(domain));
    let mut at: HashMap<String, u64> = following
        .as_ref()
        .map(|following| following.followed.heads())
        .unwrap_or_default();
    let mut chains: HashMap<String, meridian_changes::integrity::Chain> = HashMap::new();
    // The lineage (contract v30, stage C; the resilience spec's section 3):
    // the change last applied of each partition, the epoch the writer's
    // control partition was last seen at, and the partitions to read back
    // since it rose. A partition unchecked takes nothing from the tail.
    let mut applied: HashMap<String, Applied> = HashMap::new();
    let mut control_epoch: Option<u64> = None;
    let mut unchecked: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    // Behind since a partition was left to read back.
    let mut unchecked_since: Option<Instant> = None;
    // Behind since a page was missed and not yet read, each partition's
    // own gap; unanswered since the writer last failed to answer. The lag
    // is the longest. A gap is let go once its partition holds the latest
    // page heard past it, or the record's head where the record no longer
    // reaches that page, however it came to -- read from the writer here,
    // by the reconcile, or a read-back's reload -- and is read again each
    // time the writer answers: never left to grow the member's lag for good
    // (CI 44187810396; review-c5c-ci1).
    let mut gaps: HashMap<String, Gap> = HashMap::new();
    let mut unanswered_since: Option<Instant> = None;
    // Whether the writer has answered yet: a reader reconciles with the
    // primary the first time it does, and again after each time it did not.
    let mut answered = false;
    let mut ask = tokio::time::interval(ASK_EVERY);
    // A tick held past the next by a read waits a second after it, never a
    // burst of the ticks missed meanwhile (review-c5i M2): reset as the held
    // tick ends, below, since Delay still fires the tick missed at once.
    ask.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            heard = tail.recv() => {
                let Some(delivery) = heard else { return };
                // TEST ONLY: a reader held behind on purpose, while the file
                // `MERIDIAN_READER_HOLD_FILE` names exists (the ha failover
                // run's check that a reader held behind answers from the
                // primary). Pages heard meanwhile are not applied; the next
                // after it is gone is read from the writer, as any gap is.
                if following.is_some() && held_on_purpose() {
                    continue;
                }
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
                // A higher epoch than the change last applied: read back
                // whether the record still holds what this member applied,
                // before taking anything more of the partition.
                let page_epoch = page.changes.iter().map(|c| c.epoch).max().unwrap_or(0);
                if applied.get(&partition).is_some_and(|(_, epoch, _)| page_epoch > *epoch) {
                    unchecked.insert(partition.clone());
                }
                if unchecked.contains(&partition) {
                    let since = behind_since(&gaps, &at, [unanswered_since, unchecked_since, Some(Instant::now())]);
                    let check = Lineage { bus: &bus, domain, following: following.as_ref() };
                    minding(
                        since,
                        bound,
                        check.read_back(&mut unchecked, &mut at, &mut applied, &mut chains, &mut gaps),
                    )
                    .await;
                    if unchecked.is_empty() {
                        unchecked_since = None;
                    }
                    if unchecked.contains(&partition) {
                        unchecked_since.get_or_insert_with(Instant::now);
                        // Not taken, and a gap: read once the read-back
                        // finds the record holding what it applied, never
                        // left unread until the partition's next page, the
                        // member ready while behind (review-c5i L3).
                        let held = at.get(&partition).copied();
                        if held.is_none_or(|held| page.head > held) {
                            Gap::found(&mut gaps, &partition, held, page.head);
                        }
                        continue;
                    }
                }
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
                        Gap::found(&mut gaps, &partition, at.get(&partition).copied(), page.head);
                        continue;
                    }
                }
                match at.get(&partition).copied() {
                    None => {
                        // Its changes before this page never followed: what
                        // a reader holds of it is dropped, and held from here.
                        if let Some(following) = &following {
                            following.apply.reset(&partition);
                        }
                        at.insert(partition.clone(), page.head);
                    }
                    Some(held) if page.head <= held => {}
                    Some(held) if page.after_sequence == held => {
                        if let Some(following) = &following {
                            following.apply.apply(&partition, &page.changes);
                        }
                        if let Some(last) = page.changes.last() {
                            applied.insert(partition.clone(), applied_of(last));
                        }
                        at.insert(partition.clone(), page.head);
                    }
                    Some(held) => {
                        // A page missed: read it from the writer, never infer it.
                        Gap::found(&mut gaps, &partition, Some(held), page.head);
                        let since = behind_since(&gaps, &at, [unanswered_since, unchecked_since]);
                        minding(
                            since,
                            bound,
                            read_gap(
                                &bus,
                                domain,
                                following.as_ref(),
                                &partition,
                                held,
                                page.head,
                                &mut Behind {
                                    at: &mut at,
                                    applied: &mut applied,
                                    chains: &mut chains,
                                    gaps: &mut gaps,
                                    unchecked: &mut unchecked,
                                    unchecked_since: &mut unchecked_since,
                                },
                            ),
                        )
                        .await;
                    }
                }
                if let (Some(following), Some(head)) = (&following, at.get(&partition)) {
                    following.followed.set(&partition, *head);
                }
                said().set_partitions(at.keys().cloned().collect());
            }
            _ = ask.tick() => {
                let ticked = Instant::now();
                match ask_writer(&bus, domain).await {
                    Ok(epoch) => {
                        // The writer's epoch rose -- a takeover, or a new
                        // database timeline (contract v30, stage C): every
                        // partition held is read back before it is followed
                        // further.
                        if control_epoch.is_some_and(|seen| epoch > seen) {
                            unchecked.extend(at.keys().cloned());
                        }
                        control_epoch = Some(epoch);
                        // A partition left to read back took nothing from
                        // the tail meanwhile, and a writer whose epoch rose
                        // -- a new writer -- published none of what it
                        // committed before this follower first asked it: a
                        // reader reconciles once they are read back.
                        let read_back = !unchecked.is_empty();
                        if read_back {
                            unchecked_since.get_or_insert_with(Instant::now);
                            let since = behind_since(&gaps, &at, [unanswered_since, unchecked_since]);
                            let check = Lineage { bus: &bus, domain, following: following.as_ref() };
                            minding(
                                since,
                                bound,
                                check.read_back(&mut unchecked, &mut at, &mut applied, &mut chains, &mut gaps),
                            )
                            .await;
                        }
                        if unchecked.is_empty() {
                            unchecked_since = None;
                        }
                        let unanswered = unanswered_since.take();
                        let again = unanswered.is_some() || !answered;
                        answered = true;
                        if let Some(following) = &following {
                            // Held behind on purpose (TEST ONLY): what it
                            // lacks is read once it is let go.
                            let held = held_on_purpose();
                            let left_behind = if held {
                                None
                            } else {
                                bus.reads_after().left_behind(domain)
                            };
                            if again || (!held && read_back) || left_behind.is_some() {
                                // A read left behind proves the reader behind
                                // from when that read came, which waited its
                                // bound (review-delta A3): minded from then,
                                // never held ready while the reconcile reads
                                // (review-c5m2 2).
                                let proved = Instant::now();
                                let evidence = [unanswered, unchecked_since, left_behind];
                                let since = behind_since(&gaps, &at, evidence);
                                // A gap the reconcile records is behind from
                                // this evidence alone, never from another
                                // partition's gap (review-delta A2).
                                let behind_from = evidence
                                    .into_iter()
                                    .flatten()
                                    .min()
                                    .unwrap_or(proved);
                                // Minded from now where nothing says it was
                                // behind before: every catch-up may wait its
                                // read's timeout, and the member is never held
                                // ready past its bound meanwhile (review-delta
                                // A1).
                                let reconciled = minding(
                                    since.or(Some(proved)),
                                    bound,
                                    reconcile(
                                        &bus,
                                        domain,
                                        following,
                                        behind_from,
                                        &mut Behind {
                                            at: &mut at,
                                            applied: &mut applied,
                                            chains: &mut chains,
                                            gaps: &mut gaps,
                                            unchecked: &mut unchecked,
                                            unchecked_since: &mut unchecked_since,
                                        },
                                    ),
                                )
                                .await;
                                // The primary's heads unread: the proof of
                                // being behind carried on, as the writer's
                                // silence is, so the lag counts it and the
                                // next tick reconciles again (review c5w
                                // R1) -- never ready while proven behind.
                                if !reconciled {
                                    unanswered_since = Some(
                                        unanswered_since.map_or(behind_from, |since| {
                                            since.min(behind_from)
                                        }),
                                    );
                                }
                            }
                        }
                        // A gap still open is read again as the writer
                        // answers: a catch-up that failed while it did -- a
                        // read timed out under load -- is otherwise read
                        // again only by its partition's next page, which a
                        // partition seldom written may not send for hours
                        // (review-c5c-ci1 M2). The gap read least recently
                        // goes first, and none is begun a second into the
                        // pass: a read that times out holds the loop 5 s,
                        // and reading every gap held it 5 s a gap, the tail
                        // undrained (review-c5i M2).
                        if !(following.is_some() && held_on_purpose()) {
                            let mut open: Vec<(Option<Instant>, String, Option<u64>, u64)> = gaps
                                .iter()
                                .filter(|(partition, _)| !unchecked.contains(*partition))
                                .filter_map(|(partition, gap)| {
                                    let held = at.get(partition).copied();
                                    gap.open(held)
                                        .then(|| (gap.tried, partition.clone(), held, gap.to))
                                })
                                .collect();
                            open.sort();
                            let pass = Instant::now();
                            for (_, partition, held, to) in open {
                                if pass.elapsed() >= ASK_EVERY {
                                    break;
                                }
                                let since = behind_since(&gaps, &at, [unchecked_since]);
                                minding(
                                    since,
                                    bound,
                                    read_again(
                                        &bus,
                                        domain,
                                        following.as_ref(),
                                        &partition,
                                        held,
                                        to,
                                        &mut Behind {
                                            at: &mut at,
                                            applied: &mut applied,
                                            chains: &mut chains,
                                            gaps: &mut gaps,
                                            unchecked: &mut unchecked,
                                            unchecked_since: &mut unchecked_since,
                                        },
                                    ),
                                )
                                .await;
                                if let Some(gap) = gaps.get_mut(&partition) {
                                    gap.tried = Some(Instant::now());
                                }
                            }
                            said().set_partitions(at.keys().cloned().collect());
                        }
                    }
                    Err(failed) => {
                        if unanswered_since.is_none() {
                            tracing::warn!(domain, %failed, "the writer's change log did not answer");
                        }
                        unanswered_since.get_or_insert_with(Instant::now);
                    }
                }
                gaps.retain(|partition, gap| gap.open(at.get(partition).copied()));
                let lag = behind_since(&gaps, &at, [unanswered_since, unchecked_since])
                    .map_or(0, |since| since.elapsed().as_millis() as u64);
                said().set_lag_ms(lag);
                crate::mark_ready(Duration::from_millis(lag) < bound);
                // Held past the next tick by a read: the next a second from
                // now. Fired at once, it began another read before a page
                // heard meanwhile was taken from the tail, and held the loop a
                // tick and two reads, past review-c5i M2's pass of a second
                // and one read.
                if ticked.elapsed() >= ASK_EVERY {
                    ask.reset();
                }
            }
        }
    }
}

/// A page of a partition missed and not yet read: since when, and the head
/// of the latest page heard past it -- where the gap ends, unless the
/// record no longer reaches that far.
struct Gap {
    since: Instant,
    to: u64,
    /// When it was last read again, as the writer answered.
    tried: Option<Instant>,
    /// What a read of it that failed was last warned of -- a change refused
    /// by its digest or chain (true), or a read that failed otherwise: once
    /// a kind, as an unanswered writer is, not at each read again (review-c5i
    /// L6), and again when the kind changes (review-c5m2 5).
    warned: Option<bool>,
}

impl Gap {
    /// A read of `partition`'s gap that failed, said the first time its
    /// kind is met, and at debug after: a gap that will not close -- a chain
    /// or a digest refused, a writer that keeps timing out -- is read again
    /// each second the writer answers. A change refused ([`REFUSED`]) is
    /// said at error, after a timeout warned of too: a change corrupt is
    /// found and said (section 9), never at debug behind the failure before
    /// it (review-c5m2 5).
    fn failed(
        gaps: &mut HashMap<String, Gap>,
        domain: &str,
        partition: &str,
        failed: &str,
        what: &str,
    ) {
        let refused = failed.starts_with(REFUSED);
        let said = gaps
            .get_mut(partition)
            .and_then(|gap| gap.warned.replace(refused));
        if said == Some(refused) {
            tracing::debug!(domain, partition, %failed, "{what}");
        } else if refused {
            tracing::error!(domain, partition, %failed, "{what}");
        } else {
            tracing::warn!(domain, partition, %failed, "{what}");
        }
    }

    /// A page heard past where `partition` stands at `held`, its head `to`:
    /// the gap still open reaching to this page too, or one opened from
    /// now -- never a gap let go meanwhile, its since and its end another
    /// gap's (review-c5c-ci1 H1: one found again after a reload below it
    /// was let go as below it, the reader ready while behind).
    fn found(gaps: &mut HashMap<String, Gap>, partition: &str, held: Option<u64>, to: u64) {
        match gaps.get_mut(partition) {
            Some(gap) if gap.open(held) => gap.to = gap.to.max(to),
            _ => {
                gaps.insert(
                    partition.to_string(),
                    Gap {
                        since: Instant::now(),
                        to,
                        tried: None,
                        warned: None,
                    },
                );
            }
        }
    }

    /// The record's head of `partition` learnt: its gap ends there at the
    /// latest, a page past it lost with the record's tail. A partition
    /// held at it holds all the record does, however it came to.
    fn ends_at(gaps: &mut HashMap<String, Gap>, partition: &str, head: u64) {
        if let Some(gap) = gaps.get_mut(partition) {
            gap.to = gap.to.min(head);
        }
    }

    /// Still open where its partition now stands at `held`: not yet at its
    /// end, or not yet held at all -- a partition first heard whose page did
    /// not verify is not followed, and the member stays behind, unready,
    /// until it is held (review-c5c-ci1 L1: let go at the next tick, the
    /// member ready with the page refused), from the record's head as the
    /// writer answers (review-c5i M3).
    fn open(&self, held: Option<u64>) -> bool {
        held.is_none_or(|held| held < self.to)
    }
}

/// Since when a follower has been behind, the longest it has been: its
/// oldest gap still open, or since each of `also` -- its writer unanswered, a
/// partition left to read back. Its lag is the time since.
fn behind_since(
    gaps: &HashMap<String, Gap>,
    at: &HashMap<String, u64>,
    also: impl IntoIterator<Item = Option<Instant>>,
) -> Option<Instant> {
    gaps.iter()
        .filter(|(partition, gap)| gap.open(at.get(*partition).copied()))
        .map(|(_, gap)| gap.since)
        .chain(also.into_iter().flatten())
        .min()
}

/// A read awaited within the follow loop, the member's readiness kept to its
/// lag meanwhile: behind since `since`, it says it is unready once that
/// passes its `bound`, and its lag each second after, not once the read ends
/// -- a read timing out held the loop, and the member said it was ready, as
/// the last tick left it, for seconds past its bound (review-c5i M2).
async fn minding<T>(
    since: Option<Instant>,
    bound: Duration,
    read: impl std::future::Future<Output = T>,
) -> T {
    let Some(since) = since else {
        return read.await;
    };
    tokio::pin!(read);
    let mut every = tokio::time::interval_at((since + bound).into(), ASK_EVERY);
    // Once a tick from now, never every tick missed since the bound passed:
    // a gap open long is read each second, and each tick missed was said at
    // once, the readiness file written each time (review-c5m2 1).
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            done = &mut read => return done,
            _ = every.tick() => {
                #[cfg(test)]
                MINDED.with(|minded| minded.set(minded.get() + 1));
                said().set_lag_ms(since.elapsed().as_millis() as u64);
                crate::mark_ready(false);
            }
        }
    }
}

#[cfg(test)]
thread_local! {
    /// How many times [`minding`] said its lag, on this thread (tests only).
    static MINDED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// What a follower holds and is behind on, each partition's: where it
/// stands, the change it last applied, the book's chain, its gaps, and the
/// partitions left to read back, since when.
struct Behind<'a> {
    at: &'a mut HashMap<String, u64>,
    applied: &'a mut HashMap<String, Applied>,
    chains: &'a mut HashMap<String, meridian_changes::integrity::Chain>,
    gaps: &'a mut HashMap<String, Gap>,
    unchecked: &'a mut std::collections::BTreeSet<String>,
    unchecked_since: &'a mut Option<Instant>,
}

/// `partition` handed to the read-back, the member behind since its gap was
/// found where it had one: the read-back's own wait measured from now, the
/// lag fell to nothing, and the member said it was ready while it held what
/// the record lost and the read-back went unanswered -- a gap the reconcile
/// then ended at the record's head, below where the partition stood, gone
/// too (review-c5i L1).
fn hand_to_read_back(
    unchecked: &mut std::collections::BTreeSet<String>,
    unchecked_since: &mut Option<Instant>,
    gaps: &HashMap<String, Gap>,
    partition: &str,
) {
    unchecked.insert(partition.to_string());
    let since = gaps
        .get(partition)
        .map_or_else(Instant::now, |gap| gap.since);
    *unchecked_since = Some(unchecked_since.map_or(since, |was| was.min(since)));
}

/// A gap still open read again as the writer answers: from `held` where its
/// partition stands to `to`; or, where the partition is not yet held -- first
/// heard, its page refused -- the partition held from the record's head, as
/// one first heard is held from its page's: what a reader held of it dropped,
/// no change applied and no chain, so the next change is taken as it is.
/// Never left waiting for a page of it that verifies, which a partition
/// seldom written may not send for hours, the member unready meanwhile and a
/// drain of the writer's node refused by its budget (review-c5i M3).
async fn read_again(
    bus: &Bus,
    domain: &'static str,
    following: Option<&Following>,
    partition: &str,
    held: Option<u64>,
    to: u64,
    behind: &mut Behind<'_>,
) {
    if let Some(held) = held {
        return read_gap(bus, domain, following, partition, held, to, behind).await;
    }
    let head = match holds(bus, domain, partition, i64::MAX as u64, None).await {
        Ok((_, head)) => head,
        Err(failed) => {
            Gap::failed(
                behind.gaps,
                domain,
                partition,
                &failed,
                "a partition first heard whose page was refused could not be held from the record's head",
            );
            return;
        }
    };
    if let Some(following) = following {
        following.apply.reset(partition);
        following.followed.set(partition, head);
    }
    behind.chains.remove(partition);
    behind.applied.remove(partition);
    behind.gaps.remove(partition);
    behind.at.insert(partition.to_string(), head);
}

/// A gap of `partition` read from the writer, from `held` where it stands
/// to its end, `heard` the head of the latest page heard past it: read
/// and let go; handed to the read-back where the record no longer reaches
/// what was applied, or the page heard; held again from `heard` past the
/// writer's retention; or left open, to be read again.
async fn read_gap(
    bus: &Bus,
    domain: &'static str,
    following: Option<&Following>,
    partition: &str,
    held: u64,
    heard: u64,
    behind: &mut Behind<'_>,
) {
    let applying = following.map(|f| f.apply.as_ref());
    let last = behind.applied.get(partition).cloned();
    let (reached, read) = catch_up(
        bus,
        domain,
        partition,
        held,
        last.as_ref(),
        behind.chains,
        applying,
    )
    .await;
    // What it applied before any failure is held: the partition stands there.
    if let Some(last) = reached.last {
        behind.applied.insert(partition.to_string(), last);
        behind.at.insert(partition.to_string(), reached.head);
    }
    match read {
        // Held at the record's head, never at the page heard past it: where
        // the record's head is below that page, the record lost it -- a
        // failover -- or its writer answers behind, and the read-back
        // decides. Held at the page heard, the partition claimed changes it
        // did not hold, and its reads were answered from without them
        // (review-c5i M4).
        Ok(()) => {
            behind.at.insert(partition.to_string(), reached.head);
            if reached.head < heard {
                tracing::warn!(domain, partition, head = reached.head, heard, "the record's head is below a page heard: what this member applied is read back from the record");
                hand_to_read_back(
                    behind.unchecked,
                    behind.unchecked_since,
                    behind.gaps,
                    partition,
                );
            } else {
                behind.gaps.remove(partition);
            }
        }
        // The record no longer reaches what this member applied, or no
        // longer holds the change it last applied: read it back (contract
        // v30, stage C; review-c5i M1), the member behind since the gap was
        // found ([`hand_to_read_back`]). The gap is kept, and read again
        // once the read-back has ended it at the record's head, where the
        // record holds what was applied.
        Err(failed) if failed.starts_with(DIVERGED) || failed.starts_with(LINEAGE) => {
            tracing::warn!(domain, partition, %failed, "a page missed could not be caught up: what this member applied is read back from the record");
            hand_to_read_back(
                behind.unchecked,
                behind.unchecked_since,
                behind.gaps,
                partition,
            );
        }
        // Behind the writer's retention: the partition held again from the
        // page heard, by a reader and a standby alike -- a standby left with
        // the gap open was unready until a takeover or an epoch rise, the gap
        // read again every second (review-c5i L2).
        Err(failed) if meridian_changes::is_behind(&failed) => {
            if let Some(following) = following {
                following.apply.reset(partition);
            }
            behind.chains.remove(partition);
            behind.applied.remove(partition);
            behind.at.insert(partition.to_string(), heard);
            behind.gaps.remove(partition);
        }
        Err(failed) => Gap::failed(
            behind.gaps,
            domain,
            partition,
            &failed,
            "a page missed could not be read",
        ),
    }
    if let (Some(following), Some(head)) = (following, behind.at.get(partition)) {
        following.followed.set(partition, *head);
    }
}

/// A reader reconciled with its store's primary, as its writer answers it
/// for the first time or again: the writer publishes its change log's tail
/// only while a follower asks (ruling 1062), so what it committed before --
/// a reader started before its writer, or a writer restarted -- was never
/// heard. Each partition the primary holds that the reader does not is held
/// from its head, as one first heard is; one it is behind on is read from
/// the writer, as any gap is. Without this a partition written only then
/// was never followed, and every read naming it waited out its bound and
/// was answered from the primary (CI 42868480584, the book's reader).
///
/// And twice more, since a page missed is otherwise learnt only from the
/// partition's next page, which a partition seldom written -- a store's
/// `control` -- may not send for hours (failover-repeat 43697279874, ha run
/// 2, the book's reader):
///
/// - **once a new writer is read back** (its epoch rose): a writer that took
///   over cleanly answered every ask, so nothing above ran, and what it
///   committed before this follower first asked it -- its own takeover,
///   journalled on `control` as it starts -- was published to nobody;
/// - **once a read was answered from the primary past its wait**
///   ([`Reads::left_behind`]): proof the reader is behind there, whatever
///   lost the page.
///
/// A partition still to be read back (`unchecked`) is left: what the reader
/// holds of it is not yet known to be the record's. What it applies is the
/// change last applied of the partition, as the tail's is, so a later
/// read-back checks it; one held again from the head applied none
/// (review-c5c-ci1 M3: a stale one read back as kept, and the reader held
/// what the record had lost, ready).
///
/// Says whether it read the primary's heads: where it could not, nothing
/// was reconciled, and its caller carries `behind_from` on (review c5w R1).
///
/// [`Reads::left_behind`]: meridian_bus::read_after::Reads::left_behind
async fn reconcile(
    bus: &Bus,
    domain: &'static str,
    following: &Following,
    behind_from: Instant,
    behind: &mut Behind<'_>,
) -> bool {
    let Behind {
        at,
        applied,
        chains,
        gaps,
        unchecked,
        unchecked_since,
    } = behind;
    // Read off the runtime's own threads: a store's record is read blocking
    // (the book's and the lake's Postgres clients refuse to be driven from
    // an async task; CI 42878357895).
    let primary = Arc::clone(&following.primary);
    let heads = match tokio::task::spawn_blocking(move || primary.all_heads()).await {
        Ok(Some(heads)) => heads,
        Ok(None) => return true,
        Err(failed) => {
            tracing::warn!(domain, %failed, "the primary's heads could not be read to reconcile with");
            return false;
        }
    };
    let heads = match heads {
        Ok(heads) => heads,
        Err(failed) => {
            tracing::warn!(domain, %failed, "the primary's heads could not be read to reconcile with");
            return false;
        }
    };
    for (partition, head) in heads {
        // A gap ends at the record's head at the latest: a page past it was
        // lost with the record's tail (review-c5c-ci1 M1).
        Gap::ends_at(gaps, &partition, head);
        if unchecked.contains(&partition) {
            continue;
        }
        let reached = match at.get(&partition).copied() {
            Some(held) if held >= head => continue,
            Some(held) => {
                let last = applied.get(&partition).cloned();
                let (reached, read) = catch_up(
                    bus,
                    domain,
                    &partition,
                    held,
                    last.as_ref(),
                    chains,
                    Some(following.apply.as_ref()),
                )
                .await;
                let progressed = reached.last.is_some();
                if let Some(last) = reached.last {
                    applied.insert(partition.clone(), last);
                }
                match read {
                    Ok(()) => reached.head.max(held),
                    Err(failed) if meridian_changes::is_behind(&failed) => {
                        following.apply.reset(&partition);
                        chains.remove(&partition);
                        applied.remove(&partition);
                        head
                    }
                    // The record no longer holds what it applied: the
                    // read-back decides first, held as far as it was read
                    // and applied meanwhile (review-c5i M1).
                    Err(failed) if failed.starts_with(DIVERGED) || failed.starts_with(LINEAGE) => {
                        tracing::warn!(domain, partition, %failed, "a partition behind the primary could not be caught up: what this member applied is read back from the record");
                        hand_to_read_back(unchecked, unchecked_since, gaps, &partition);
                        if !progressed {
                            continue;
                        }
                        reached.head
                    }
                    // Held as far as it was read and applied, and a gap to
                    // the primary's head, behind since `behind_from`: read
                    // again as the writer answers, the member unready while
                    // it is open -- never behind with no gap, its lag 0, ready
                    // until another read was left behind or the partition's
                    // next page came (review-c5m2 2).
                    Err(failed) => {
                        tracing::warn!(domain, partition, %failed, "a partition behind the primary could not be read");
                        let stands = if progressed { reached.head } else { held };
                        Gap::found(gaps, &partition, Some(stands), head);
                        if let Some(gap) = gaps.get_mut(&partition) {
                            gap.since = gap.since.min(behind_from);
                        }
                        if !progressed {
                            continue;
                        }
                        reached.head
                    }
                }
            }
            None => {
                following.apply.reset(&partition);
                chains.remove(&partition);
                applied.remove(&partition);
                head
            }
        };
        at.insert(partition.clone(), reached);
        following.followed.set(&partition, reached);
    }
    said().set_partitions(at.keys().cloned().collect());
    true
}

/// Whether a reader is held behind on purpose: TEST ONLY, by the file
/// `MERIDIAN_READER_HOLD_FILE` names, which the chart never sets.
fn held_on_purpose() -> bool {
    var("MERIDIAN_READER_HOLD_FILE").is_some_and(|path| std::path::Path::new(&path).exists())
}

/// Ask the writer's change log, for its control partition from its head:
/// cheap, and it is the interest that keeps the tail published.
async fn ask_writer(bus: &Bus, domain: &str) -> Result<u64, String> {
    let asked = meridian_changes::ReadChangesRequest {
        partition: meridian_changes::CONTROL.to_string(),
        after_sequence: i64::MAX as u64,
        page_size: 1,
    };
    let (_, body) = bus
        .call(
            &meridian_changes::read_changes_topic(domain),
            "meridian.v1.ReadChangesRequest",
            asked.encode_to_vec(),
            None,
            Some(ASK_EVERY),
        )
        .await
        .map_err(|failed| failed.to_string())?;
    meridian_changes::ReadChangesReply::decode(body.as_slice())
        .map(|reply| reply.epoch)
        .map_err(|failed| failed.to_string())
}

/// What a follower last applied of a partition: the change's sequence, its
/// epoch and its body's digest (contract v30, stage C).
type Applied = (u64, u64, Vec<u8>);

fn applied_of(change: &meridian_changes::Change) -> Applied {
    (
        change.sequence,
        change.epoch,
        meridian_changes::integrity::digest(&change.body),
    )
}

/// The words a catch-up refuses with where the record's head is below what
/// the follower holds: the record lost what it applied.
const DIVERGED: &str = "the record's head is below what this member applied";

/// The words a catch-up refuses with where the record no longer holds the
/// change the follower last applied, as a change of a higher epoch than it
/// showed: a database failover lost a tail it applied, and the writer took
/// those sequences again.
const LINEAGE: &str = "the record no longer holds the change this member last applied";

/// A catch-up's page refused by a change's digest, or the book's chain: a
/// change corrupt, said as such ([`Gap::failed`]; review-c5m2 5).
const REFUSED: &str = "a change did not verify";

/// Changes followers of this process dropped, held past what the record
/// holds after a database failover or restore lost them (contract v30,
/// stage C): what `status` says of the lost committed tail.
pub static DROPPED: AtomicU64 = AtomicU64::new(0);

/// A follower reading back what it applied, once the writer's epoch rose
/// (contract v30, stage C; the resilience spec's section 3; Kafka's
/// KIP-101). The record keeps every change's epoch; where it still holds
/// the change this member last applied of a partition -- its sequence,
/// epoch and body -- nothing it applied was lost, and it follows on. Where
/// it does not -- the database's failover or restore lost a tail this
/// member had applied, and the writer took those sequences again -- what
/// it holds of the partition is dropped and the partition reloaded at the
/// record's head (a reader's hot layer holds each key's latest, never the
/// changes, so it cannot keep what came before and drop only the rest),
/// so it never holds a change the record does not.
struct Lineage<'a> {
    bus: &'a Bus,
    domain: &'static str,
    following: Option<&'a Following>,
}

impl Lineage<'_> {
    async fn read_back(
        &self,
        unchecked: &mut std::collections::BTreeSet<String>,
        at: &mut HashMap<String, u64>,
        applied: &mut HashMap<String, Applied>,
        chains: &mut HashMap<String, meridian_changes::integrity::Chain>,
        gaps: &mut HashMap<String, Gap>,
    ) {
        let domain = self.domain;
        for partition in unchecked.clone() {
            let held = at.get(&partition).copied().unwrap_or(0);
            let last = applied.get(&partition).cloned();
            let found = match holds(self.bus, domain, &partition, held, last.as_ref()).await {
                Ok(found) => found,
                // The writer refused to read it back -- a change of the
                // record no longer matching its digest (bit rot), say --
                // so what this member applied cannot be shown to be the
                // record's: held from the record's head, as one lost is
                // (CI 42965355676: the conductor's standby, asking for ever,
                // stayed unready).
                Err(failed) if failed.contains("handler for") => {
                    match holds(self.bus, domain, &partition, i64::MAX as u64, None).await {
                        Ok((_, head)) => {
                            tracing::warn!(domain, partition, %failed, "what this member applied could not be read back from the record: held from its head");
                            (false, head)
                        }
                        Err(again) => {
                            tracing::warn!(domain, partition, %again, "what this member applied could not be read back; asking again");
                            continue;
                        }
                    }
                }
                Err(failed) => {
                    tracing::warn!(domain, partition, %failed, "what this member applied could not be read back; asking again");
                    continue;
                }
            };
            unchecked.remove(&partition);
            let (kept, head) = found;
            // Kept or reloaded, the partition holds all the record does to
            // its head, and a gap found before ends there at the latest: a
            // page past it was lost with the record's tail, and is never
            // waited for (review-c5c-ci1 M1).
            Gap::ends_at(gaps, &partition, head);
            if kept {
                continue;
            }
            let dropped = held.saturating_sub(head).max(1);
            DROPPED.fetch_add(dropped, Ordering::Relaxed);
            tracing::warn!(
                domain,
                partition,
                held,
                head,
                "the record no longer holds what this member applied (a database failover or restore lost it): dropped, the partition reloaded at the record's head"
            );
            if let Some(following) = self.following {
                following.apply.reset(&partition);
                following.followed.set(&partition, head);
            }
            chains.remove(&partition);
            applied.remove(&partition);
            at.insert(partition, head);
        }
    }
}

/// Whether the record still holds what a follower stands at in a partition,
/// and its head: the change it last applied read back from the writer, the
/// same sequence, epoch and body, and the record's head not below where it
/// stands; or where it applied none, that head alone. Past the record's
/// retention, not held.
async fn holds(
    bus: &Bus,
    domain: &str,
    partition: &str,
    at: u64,
    applied: Option<&Applied>,
) -> Result<(bool, u64), String> {
    let ask = |after: u64| meridian_changes::ReadChangesRequest {
        partition: partition.to_string(),
        after_sequence: after,
        page_size: 1,
    };
    let call = |asked: meridian_changes::ReadChangesRequest| async move {
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
        meridian_changes::ReadChangesReply::decode(body.as_slice())
            .map_err(|failed| failed.to_string())
    };
    let after = applied.map_or(at, |(sequence, _, _)| sequence.saturating_sub(1));
    let reply = match call(ask(after)).await {
        Ok(reply) => reply,
        Err(failed) if meridian_changes::is_behind(&failed) => {
            let head = call(ask(i64::MAX as u64)).await?.head;
            return Ok((false, head));
        }
        Err(failed) => return Err(failed),
    };
    let kept = match applied {
        Some((sequence, epoch, digest)) => {
            reply.head >= at
                && reply.changes.first().is_some_and(|change| {
                    change.sequence == *sequence
                        && change.epoch == *epoch
                        && meridian_changes::integrity::digest(&change.body) == *digest
                })
        }
        None => reply.head >= at,
    };
    Ok((kept, reply.head))
}

/// Each change verified by its digest, and the book's by its chain too: the
/// chain moved only where every change verifies, so a page refused leaves
/// it where the partition stands (review-c5c-ci1 M4, L1).
fn verified(
    domain: &str,
    chains: &mut HashMap<String, meridian_changes::integrity::Chain>,
    changes: &[meridian_changes::Change],
) -> Result<(), String> {
    let mut moved: HashMap<String, meridian_changes::integrity::Chain> = HashMap::new();
    for change in changes {
        if domain == "book" {
            moved
                .entry(change.partition.clone())
                .or_insert_with(|| chains.get(&change.partition).cloned().unwrap_or_default())
                .follow(change)?;
        } else {
            meridian_changes::integrity::verify(change)?;
        }
    }
    chains.extend(moved);
    Ok(())
}

/// How far a catch-up came: where the partition stands, and the change it
/// last applied, where it applied any.
struct Reached {
    head: u64,
    last: Option<Applied>,
}

/// Read a partition's changes after `from` from the writer, page by page,
/// to its head, each page verified and applied before the next is read.
/// How far it came is answered whether or not it reached the head: a page
/// applied, then a read that failed, leaves the partition, its last change
/// and the book's chain at that page, never the chain ahead of where it
/// stands (review-c5c-ci1 M4: the next catch-up, from where it stood,
/// refused for good as not following).
///
/// A page of a higher epoch than the record is known to hold what this
/// member applied through -- `applied`, the change it last applied, or one
/// this catch-up applied -- is applied only once the record is read back to
/// hold that change still, as Kafka's follower asks its leader where its
/// last epoch ends before it fetches on (KIP-101, KIP-279), and the page is
/// read again from there. Where the record does not hold it, the catch-up
/// stops before the page, refused with [`LINEAGE`]: a database failover lost
/// a tail it applied, and the read-back drops it. Applied across, it kept
/// a change the record lost, the read-back after found the record holding
/// what it applied last and kept it all, and the lake's reader, checked by
/// digests alone, held it for good (review-c5i M1).
async fn catch_up(
    bus: &Bus,
    domain: &str,
    partition: &str,
    from: u64,
    applied: Option<&Applied>,
    chains: &mut HashMap<String, meridian_changes::integrity::Chain>,
    apply: Option<&dyn Apply>,
) -> (Reached, Result<(), String>) {
    let mut reached = Reached {
        head: from,
        last: None,
    };
    let read = read_to_head(
        bus,
        domain,
        partition,
        from,
        applied,
        chains,
        apply,
        &mut reached,
    )
    .await;
    (reached, read)
}

#[allow(clippy::too_many_arguments)]
async fn read_to_head(
    bus: &Bus,
    domain: &str,
    partition: &str,
    from: u64,
    applied: Option<&Applied>,
    chains: &mut HashMap<String, meridian_changes::integrity::Chain>,
    apply: Option<&dyn Apply>,
    reached: &mut Reached,
) -> Result<(), String> {
    let mut after = from;
    // The epoch through which the record is known to hold what this member
    // applied: the change it last applied's, at first.
    let mut known = applied.map(|(_, epoch, _)| *epoch);
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
        // Below what this member stands at: the record lost what it applied
        // (contract v30, stage C).
        if after == from && page.head < from {
            return Err(format!("{DIVERGED}: head {}, applied {from}", page.head));
        }
        let mut next = after;
        for change in &page.changes {
            if change.sequence <= next {
                // Refused, as a change that does not verify is: said at
                // error, never at debug behind a timeout warned of before
                // (review-delta A4).
                return Err(format!(
                    "{REFUSED}: change {} came after {next}: out of order",
                    change.sequence
                ));
            }
            next = change.sequence;
        }
        let rising = page.changes.iter().map(|change| change.epoch).max();
        if let (Some(through), Some(rising)) = (known, rising) {
            if rising > through {
                let last = reached.last.as_ref().or(applied);
                let (kept, head) = holds(bus, domain, partition, after, last).await?;
                if !kept {
                    return Err(format!(
                        "{LINEAGE}: {after} at epoch {through}, the record's head {head} at epoch {rising}"
                    ));
                }
                known = Some(rising);
                continue;
            }
        }
        verified(domain, chains, &page.changes).map_err(|failed| format!("{REFUSED}: {failed}"))?;
        if let Some(apply) = apply {
            apply.apply(partition, &page.changes);
        }
        after = next;
        known = known.max(rising);
        if let Some(change) = page.changes.last() {
            reached.head = after;
            reached.last = Some(applied_of(change));
        }
        if page.changes.is_empty() || after >= page.head {
            reached.head = after.max(page.head);
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests;
