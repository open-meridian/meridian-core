//! A writer's lease: a `coordination.k8s.io/v1` Lease in the release's
//! namespace, which says which member of a component should write (step 3b;
//! spec/a-deployment-scales-out-and-fails-over-by-configuration sections 1 to
//! 3; rulings 1046, 1056, 1060).
//!
//! The cluster's own consensus, borrowed: no coordinator for a firm to run,
//! and a namespaced grant only (decisions/016). A lease is **liveness only**.
//! The fence is the partition's epoch, which the store that is the record
//! checks in every commit (meridian-changes' `Fence`, the captured stores'
//! trigger): a holder that lost its lease without knowing it -- a paused
//! machine, a partition -- is refused by the epoch, whatever it believes.
//!
//! As client-go's leader election does it, and for the same reasons:
//! - **expiry is observed, never compared across clocks.** A member does not
//!   read another's `renewTime` against its own clock; it notes when it last
//!   saw the record change, by its own monotonic clock, and holds the lease
//!   expired once it has not changed for `leaseDurationSeconds`;
//! - **every write is conditional** on the `resourceVersion` read, so two
//!   members taking an expired lease at once cannot both succeed;
//! - **a holder that cannot renew writes on** (contract v30, stage C; the
//!   resilience spec's section 2, approved 2026-11-10): a renewal error
//!   alone -- the API server not answering -- never moves it, as Patroni's
//!   failsafe_mode keeps a primary through its store's outage; it steps
//!   down when the lease shows another holder or a commit is refused by the
//!   epoch. A lease lasts at least three renewals, each giving up within
//!   one renew ([`Settings::check`]);
//! - **a holder stopping releases it**, so a planned stop hands over at once.

use std::time::{Duration, Instant};

/// How a member takes and keeps its lease, from the chart's values
/// (`leases.renewSeconds`, `leases.durationSeconds`: 2 and 6).
#[derive(Debug, Clone)]
pub struct Settings {
    /// The Lease's name: one per partition group, `<release>-<component>`.
    pub name: String,
    /// This member: its pod's name.
    pub holder: String,
    /// How often the holder renews, and a standby looks.
    pub renew: Duration,
    /// How long a lease lasts unrenewed.
    pub duration: Duration,
}

impl Settings {
    /// The longest a holder goes without renewing before it steps down: the
    /// lease's duration less one renew, so it has stopped writing before any
    /// standby can see the lease lapse.
    pub fn renew_deadline(&self) -> Duration {
        self.duration.saturating_sub(self.renew).max(self.renew)
    }

    /// How often a standby looks at the lease: twice per renew, so it sees a
    /// lapse within half a renew of it happening.
    pub fn look_every(&self) -> Duration {
        (self.renew / 2).max(Duration::from_millis(250))
    }

    /// Refused where the numbers cannot work: a lease shorter than three
    /// renewals (contract v30, stage C; the resilience spec's section 2,
    /// approved 2026-11-10) -- Patroni's `loop_wait + 2 x retry_timeout <=
    /// ttl`, each renewal giving up within one renew -- leaves a standby
    /// taking a lease its holder is still renewing through one slow answer.
    pub fn check(&self) -> Result<(), String> {
        if self.name.is_empty() || self.holder.is_empty() {
            return Err("a lease needs its name and this member's name".into());
        }
        if self.renew.is_zero() || self.duration < self.renew * 3 {
            return Err(format!(
                "leases.durationSeconds ({}) must be at least three times leases.renewSeconds ({}), \
                 which must be above 0: a holder renews at least three times within its lease, \
                 each renewal giving up within one renew",
                self.duration.as_secs(),
                self.renew.as_secs()
            ));
        }
        Ok(())
    }
}

/// A Lease as read: what its spec says, and the version a write is
/// conditional on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub holder: String,
    pub duration_seconds: u64,
    pub acquire_time: String,
    pub renew_time: String,
    pub transitions: u64,
    pub resource_version: String,
    /// The Lease object's `metadata.uid`, the API server's: one Lease made
    /// again has another (step 3d, checkpoint 5, review c5a2 N5). Empty
    /// before a read.
    pub uid: String,
}

/// What a write of a Lease answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wrote {
    /// Written: the version now.
    Done(String),
    /// Someone wrote it first (409), or made it first: read it again.
    Conflict,
}

/// The Kubernetes API, as far as a lease needs it. The real one is
/// [`KubeLeases`]; tests stand one in memory.
pub trait Leases: Send + Sync {
    fn get(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Result<Option<Record>, String>> + Send;
    /// Create it, or answer [`Wrote::Conflict`] where it exists.
    fn create(
        &self,
        name: &str,
        record: &Record,
    ) -> impl std::future::Future<Output = Result<Wrote, String>> + Send;
    /// Replace it, conditional on `record.resource_version`.
    fn update(
        &self,
        name: &str,
        record: &Record,
    ) -> impl std::future::Future<Output = Result<Wrote, String>> + Send;
}

/// A shared API is one too: several leases read through one client.
impl<L: Leases> Leases for std::sync::Arc<L> {
    async fn get(&self, name: &str) -> Result<Option<Record>, String> {
        (**self).get(name).await
    }
    async fn create(&self, name: &str, record: &Record) -> Result<Wrote, String> {
        (**self).create(name, record).await
    }
    async fn update(&self, name: &str, record: &Record) -> Result<Wrote, String> {
        (**self).update(name, record).await
    }
}

/// Where a member stands after a look at its lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Standing {
    /// This member holds it.
    Held,
    /// Another holds it, renewing: this member waits.
    HeldBy(String),
}

/// One member's view of its lease, kept between looks.
pub struct Elector<L: Leases> {
    api: L,
    settings: Settings,
    /// The record last seen, and when it was first seen as it is.
    observed: Option<(Record, Instant)>,
    /// When this member last wrote the lease as its holder.
    renewed_at: Option<Instant>,
    /// Whom it took the lease from, the last time it took it
    /// ([`Elector::took_from`]).
    took_from: Option<Option<String>>,
    /// Whether the lease was free when it took it: released, or never held.
    took_free: bool,
    /// The last holder other than this member it saw hold the lease: whom
    /// a lease released since was released by (review c5a L3).
    last_other: Option<String>,
    /// Whether this member gave the lease up ([`Elector::release`]): it is
    /// stopping, and never takes or renews it again.
    gave_up: bool,
    /// The monotonic clock, read when a request has answered: a record is
    /// held seen, and a lapse judged, by when its read answered
    /// ([`Elector::seen_as_answered`]). None: by when the look began.
    answered: Option<fn() -> Instant>,
    /// Whether a look that failed leaves what was seen as it was
    /// ([`Elector::of_a_holder_that_stops`]); else it drops it, and an
    /// unobserved stretch is never counted toward a lapse.
    keeps_seen: bool,
}

impl<L: Leases> Elector<L> {
    pub fn new(api: L, settings: Settings) -> Self {
        Self {
            api,
            settings,
            observed: None,
            renewed_at: None,
            took_from: None,
            took_free: false,
            last_other: None,
            gave_up: false,
            answered: None,
            keeps_seen: false,
        }
    }

    /// For a lease whose holder stops once it has gone its renew deadline
    /// unrenewed, since nothing else fences it (step 3d: a plugin's shard
    /// of a role its epoch cannot stop, an order path's above all; a data
    /// shard writes on, as a store's writer does). Two things follow,
    /// neither true of a store's member, which writes on:
    /// - **a record is held seen when its read answered,** by `clock`, not
    ///   when the look began: the write it shows may have landed anywhere
    ///   between, and a lapse counted from before it lets a standby take
    ///   the lease while its holder may still act;
    /// - **a look that failed leaves what was seen as it was.** A record
    ///   read unchanged after a stretch unobserved was not renewed in it,
    ///   and its holder has stopped by its own deadline, so that stretch
    ///   counts toward its lapse: one failed read costs a standby no
    ///   second lapse.
    pub fn of_a_holder_that_stops(mut self, clock: fn() -> Instant) -> Self {
        self.answered = Some(clock);
        self.keeps_seen = true;
        self
    }

    /// A record held seen when its read answered, by `clock`, and a lapse
    /// judged by when the look that reads it again answered -- Kubernetes'
    /// observedTime, read after the Get (PO item 54, 2026-11-16: a data
    /// shard's standby). A look that failed still drops what was seen (review
    /// c5a5 F4): only [`Elector::of_a_holder_that_stops`] keeps it.
    pub fn seen_as_answered(mut self, clock: fn() -> Instant) -> Self {
        self.answered = Some(clock);
        self
    }

    /// [`Elector::of_a_holder_that_stops`], given to a member once it holds
    /// the lease (review c5a6 M1): a data shard's own Lease, whose standby
    /// counts only what it saw (F4) but whose holder keeps what it saw
    /// through a failed look -- else one API error after its take leaves
    /// it no transitions to raise its epoch by and nothing to release.
    pub fn keep_seen_through_failures(&mut self, clock: fn() -> Instant) {
        self.answered = Some(clock);
        self.keeps_seen = true;
    }

    /// When a record just read is held to have been seen: when its read
    /// answered where this lease's holder stops unrenewed, else `now`.
    fn seen_at(&self, now: Instant) -> Instant {
        self.answered.map_or(now, |clock| clock().max(now))
    }

    /// Whether this member gave the lease up: its looks take nothing from
    /// then on.
    pub fn gave_up(&self) -> bool {
        self.gave_up
    }

    /// Whom this member took the lease from: `Some(Some(holder))` from a
    /// holder whose lease lapsed, or which released it (the last holder this
    /// member saw), itself where it took back its own name's lease (a
    /// container restarted in its pod); `Some(None)` where it never saw a
    /// holder; `None` before it took it (review c5a L3).
    pub fn took_from(&self) -> Option<Option<String>> {
        self.took_from.clone()
    }

    /// Why this member last took the lease, as a store's takeover record
    /// says it, and from whom: from another holder whose lease lapsed; from
    /// one that released it (its record names nobody, as it always has);
    /// or free -- never held, or its own name's taken back after its
    /// container restarted. A release is never said to have lapsed (CI
    /// 43920761210: review c5a L3's names of a release's holder read as
    /// lapses in every store's rollout).
    pub fn takeover(&self) -> (&'static str, String) {
        match (&self.took_from, self.took_free) {
            (Some(Some(_)), true) => (meridian_changes::LEASE_RELEASED, String::new()),
            (Some(Some(holder)), false)
                if !holder.is_empty() && *holder != self.settings.holder =>
            {
                (meridian_changes::LEASE_LAPSED, holder.clone())
            }
            _ => (meridian_changes::LEASE_FREE, String::new()),
        }
    }

    /// Whether the lease was free when this member last took it: released
    /// by its holder, or never held.
    pub fn took_free(&self) -> bool {
        self.took_free
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// The Lease's `leaseTransitions` as this member last wrote or read
    /// it: one more on every take by another holder (step 3d, checkpoint 5,
    /// item 32: the floor of a plugin shard's epoch). None before a read.
    pub fn transitions(&self) -> Option<u64> {
        self.observed.as_ref().map(|(record, _)| record.transitions)
    }

    /// The Lease object's uid as this member last read it: what the lake
    /// keeps a shard's floor of transitions on (review c5a2 N5). None
    /// before a read of it.
    pub fn uid(&self) -> Option<String> {
        self.observed
            .as_ref()
            .map(|(record, _)| record.uid.clone())
            .filter(|uid| !uid.is_empty())
    }

    /// When this member last renewed as holder.
    pub fn renewed_at(&self) -> Option<Instant> {
        self.renewed_at
    }

    /// Take the lease where it is free or lapsed, renew it where held; say
    /// where this member stands. `now` is the monotonic clock, `wall_ns` the
    /// deployment's time for the record's own fields (read by people, never
    /// by an election).
    pub async fn look(&mut self, now: Instant, wall_ns: i64) -> Result<Standing, String> {
        let looked = self.look_once(now, wall_ns).await;
        if looked.is_err() && !self.keeps_seen {
            // Not watched while the API server did not answer (contract v30,
            // stage C): a lease's expiry is observed, and an unobserved
            // stretch is never counted as its holder's silence -- else a
            // standby cut off with its writer takes the lease the moment the
            // API server answers again (CI 42958621081, FAILURE=apiserver).
            self.observed = None;
        }
        looked
    }

    /// Who holds the lease now, by looking alone: nothing is written, so a
    /// member that watches never takes it (step 3d: a plugin's shard reads
    /// which of its instance's other shards live). None where nobody holds
    /// it, it was released, or its holder has not renewed for its duration
    /// as this member observed it.
    pub async fn watch(&mut self, now: Instant) -> Result<Option<String>, String> {
        let current = match self.api.get(&self.settings.name).await {
            Ok(Some(current)) => current,
            Ok(None) => {
                self.observed = None;
                return Ok(None);
            }
            Err(failed) => {
                // Unobserved, as in [`Elector::look`].
                if !self.keeps_seen {
                    self.observed = None;
                }
                return Err(failed);
            }
        };
        let at = self.seen_at(now);
        let changed = self
            .observed
            .as_ref()
            .is_none_or(|(seen, _)| !same_spec(seen, &current));
        if changed {
            self.observed = Some((current.clone(), at));
        }
        let seen_at = self.observed.as_ref().map_or(at, |(_, at)| *at);
        let lapse = Duration::from_secs(current.duration_seconds.max(1));
        if current.holder.is_empty() || at.duration_since(seen_at) >= lapse {
            return Ok(None);
        }
        Ok(Some(current.holder))
    }

    async fn look_once(&mut self, now: Instant, wall_ns: i64) -> Result<Standing, String> {
        let me = self.settings.holder.clone();
        let stamp = micro_time(wall_ns);
        let Some(current) = self.api.get(&self.settings.name).await? else {
            let record = Record {
                holder: me.clone(),
                duration_seconds: self.settings.duration.as_secs(),
                acquire_time: stamp.clone(),
                renew_time: stamp,
                transitions: 0,
                resource_version: String::new(),
                uid: String::new(),
            };
            return match self.api.create(&self.settings.name, &record).await? {
                Wrote::Done(version) => {
                    self.wrote(record, version, now);
                    self.took_from = Some(None);
                    self.took_free = true;
                    Ok(Standing::Held)
                }
                Wrote::Conflict => Ok(Standing::HeldBy(String::new())),
            };
        };
        // When this read answered: what it shows is held seen from then,
        // and a lapse is judged at then, never at when the look began.
        let at = self.seen_at(now);
        let changed = self
            .observed
            .as_ref()
            .is_none_or(|(seen, _)| !same_spec(seen, &current));
        if changed {
            self.observed = Some((current.clone(), at));
        }
        if !current.holder.is_empty() && current.holder != me {
            self.last_other = Some(current.holder.clone());
        }
        // Given up: whoever holds it, never this member again. A released
        // lease's holder is empty, free to the first member that looks, and
        // the member that released it went on looking while it stopped: it
        // took its own lease back, wrote on until it was killed, and left
        // the lease to lapse with nobody serving (failover-repeat
        // 43705797988, ha run 3: the book's writer, twice).
        if self.gave_up {
            return Ok(Standing::HeldBy(current.holder));
        }
        let seen_at = self.observed.as_ref().map_or(at, |(_, at)| *at);
        let lapse = Duration::from_secs(current.duration_seconds.max(1));
        let mine = current.holder == me;
        if !current.holder.is_empty() && !mine && at.duration_since(seen_at) < lapse {
            return Ok(Standing::HeldBy(current.holder));
        }
        let record = Record {
            holder: me.clone(),
            duration_seconds: self.settings.duration.as_secs(),
            acquire_time: if mine {
                current.acquire_time.clone()
            } else {
                stamp.clone()
            },
            renew_time: stamp,
            transitions: if mine {
                current.transitions
            } else {
                current.transitions + 1
            },
            resource_version: current.resource_version.clone(),
            uid: current.uid.clone(),
        };
        match self.api.update(&self.settings.name, &record).await? {
            Wrote::Done(version) => {
                self.wrote(record, version, now);
                if !mine {
                    // Released: from whoever this member last saw hold it.
                    self.took_free = current.holder.is_empty();
                    self.took_from = Some(Some(if self.took_free {
                        self.last_other.clone().unwrap_or_default()
                    } else {
                        current.holder
                    }));
                } else if self.took_from.is_none() {
                    // Its own name's lease, held by this pod before this
                    // process: taken back from itself.
                    self.took_free = false;
                    self.took_from = Some(Some(me));
                }
                Ok(Standing::Held)
            }
            Wrote::Conflict => {
                self.renewed_at = None;
                Ok(Standing::HeldBy(current.holder))
            }
        }
    }

    fn wrote(&mut self, mut record: Record, version: String, now: Instant) {
        record.resource_version = version;
        self.observed = Some((record, now));
        self.renewed_at = Some(now);
    }

    /// Give the lease up, where this member holds it: a planned stop hands
    /// over at once rather than after the lease lapses. Best effort.
    ///
    /// From here this member neither takes nor renews it, released or not:
    /// where the release was not written, the lease lapses on its own.
    pub async fn release(&mut self, wall_ns: i64) {
        self.gave_up = true;
        let Some((seen, _)) = self.observed.clone() else {
            return;
        };
        if seen.holder != self.settings.holder || self.renewed_at.is_none() {
            return;
        }
        let record = Record {
            holder: String::new(),
            duration_seconds: 1,
            renew_time: micro_time(wall_ns),
            ..seen
        };
        match self.api.update(&self.settings.name, &record).await {
            Ok(Wrote::Done(_)) => {
                tracing::info!(lease = %self.settings.name, "the lease is released")
            }
            Ok(Wrote::Conflict) => {}
            Err(failed) => {
                tracing::warn!(lease = %self.settings.name, %failed, "the lease could not be released; it lapses on its own")
            }
        }
        self.renewed_at = None;
    }
}

impl<L: Leases> Elector<L> {
    /// Give the lease up as a writer steps down (ruled 2026-11-16): read
    /// again, and released only where it still names this member, by a
    /// write conditional on the version read. A lease another member took
    /// meanwhile is never touched. Says whether it was released. From here
    /// this member neither takes nor renews it.
    pub async fn release_if_mine(&mut self, wall_ns: i64) -> Result<bool, String> {
        self.gave_up = true;
        self.renewed_at = None;
        let Some(current) = self.api.get(&self.settings.name).await? else {
            return Ok(false);
        };
        if current.holder != self.settings.holder {
            return Ok(false);
        }
        let record = Record {
            holder: String::new(),
            duration_seconds: 1,
            renew_time: micro_time(wall_ns),
            ..current
        };
        Ok(matches!(
            self.api.update(&self.settings.name, &record).await?,
            Wrote::Done(_)
        ))
    }
}

/// Whether two records are the same holding unrenewed: any write, a renew
/// with the same stamp among them, is a new version.
fn same_spec(a: &Record, b: &Record) -> bool {
    a.resource_version == b.resource_version
        && a.holder == b.holder
        && a.renew_time == b.renew_time
        && a.transitions == b.transitions
        && a.duration_seconds == b.duration_seconds
}

/// A Kubernetes `MicroTime`: RFC 3339 in UTC with microseconds.
pub fn micro_time(wall_ns: i64) -> String {
    let secs = wall_ns.div_euclid(1_000_000_000);
    let micros = wall_ns.rem_euclid(1_000_000_000) / 1_000;
    let days = secs.div_euclid(86_400);
    let of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{micros:06}Z",
        of_day / 3600,
        (of_day / 60) % 60,
        of_day % 60
    )
}

/// The civil date of a day count since 1970-01-01 (Howard Hinnant's
/// algorithm).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// How a [`KubeLeases`] makes its client: again after a request that did
/// not answer.
type MakeClient = Box<dyn Fn() -> Result<reqwest::Client, String> + Send + Sync>;

/// The in-cluster API, reached with the pod's own service account, for the
/// Leases of its own namespace and nothing else.
///
/// **A request that did not answer closes the client's connections**: the
/// next one dials afresh, as the kubelet closes its connections when a
/// heartbeat fails. Its client keeps one HTTP/2 connection for every
/// request; one whose packets were dropped stays in the pool, and the
/// requests after it waited on TCP's retransmission backoff for up to 12 s
/// after the API server could be reached again (CI 43973174092: a data
/// standby blind 11 s after an outage ended took a shard 10.85 s after its
/// holder was cut).
pub struct KubeLeases {
    /// The client, and how many times it was made again: a request that
    /// did not answer replaces only the client it went by (review c5a10 L1).
    http: std::sync::Mutex<(u64, reqwest::Client)>,
    make: MakeClient,
    base: String,
    namespace: String,
    token_path: String,
}

const TOKEN_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount/token";
const CA_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount/ca.crt";
const NAMESPACE_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount/namespace";

impl KubeLeases {
    /// The pod's own service account; the cluster's CA trusted, never
    /// skipped. Each request is bounded by the renew interval, so a holder
    /// cut off from the API server learns it within its deadline.
    pub fn in_cluster(timeout: Duration) -> Result<Self, String> {
        let host = std::env::var("KUBERNETES_SERVICE_HOST").map_err(|_| {
            "KUBERNETES_SERVICE_HOST is not set: a lease is taken in a pod, or not at all"
                .to_string()
        })?;
        let port = std::env::var("KUBERNETES_SERVICE_PORT").unwrap_or_else(|_| "443".into());
        let namespace = std::fs::read_to_string(NAMESPACE_PATH)
            .map_err(|failed| format!("no namespace for the lease: {failed}"))?;
        let ca = std::fs::read(CA_PATH)
            .map_err(|failed| format!("no cluster CA for the lease: {failed}"))?;
        let ca = reqwest::Certificate::from_pem(&ca)
            .map_err(|failed| format!("the cluster CA is unreadable: {failed}"))?;
        let make = move || {
            reqwest::Client::builder()
                .add_root_certificate(ca.clone())
                .timeout(timeout)
                .connect_timeout(timeout)
                .build()
                .map_err(|failed| failed.to_string())
        };
        Self::through(
            format!("https://{host}:{port}"),
            namespace.trim(),
            TOKEN_PATH,
            Box::new(make),
        )
    }

    /// The API at `base`, its client made by `make`, and made again after
    /// a request that did not answer.
    fn through(
        base: String,
        namespace: &str,
        token_path: &str,
        make: MakeClient,
    ) -> Result<Self, String> {
        Ok(Self {
            http: std::sync::Mutex::new((0, make()?)),
            make,
            base,
            namespace: namespace.to_string(),
            token_path: token_path.to_string(),
        })
    }

    /// The client now, and which it is.
    fn client(&self) -> (u64, reqwest::Client) {
        self.http
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Its connections closed: a fresh client in place of client `made`,
    /// which dials afresh. Where `made` was replaced already -- a request
    /// on it failed first -- the client since is kept, as is the old one
    /// where none can be made.
    fn dial_afresh(&self, made: u64) {
        let mut http = self
            .http
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if http.0 != made {
            return;
        }
        match (self.make)() {
            Ok(fresh) => *http = (made + 1, fresh),
            Err(failed) => {
                tracing::warn!(%failed, "the API server's client could not be made again")
            }
        }
    }

    /// The token, read at each request: a projected token is rotated under
    /// a long-running pod.
    fn token(&self) -> Result<String, String> {
        std::fs::read_to_string(&self.token_path)
            .map(|token| token.trim().to_string())
            .map_err(|failed| format!("no service account token: {failed}"))
    }

    fn path(&self, name: Option<&str>) -> String {
        match name {
            Some(name) => format!(
                "/apis/coordination.k8s.io/v1/namespaces/{}/leases/{name}",
                self.namespace
            ),
            None => format!(
                "/apis/coordination.k8s.io/v1/namespaces/{}/leases",
                self.namespace
            ),
        }
    }

    fn body(&self, name: &str, record: &Record) -> serde_json::Value {
        let mut metadata = serde_json::json!({ "name": name, "namespace": self.namespace });
        if !record.resource_version.is_empty() {
            metadata["resourceVersion"] = serde_json::json!(record.resource_version);
        }
        serde_json::json!({
            "apiVersion": "coordination.k8s.io/v1",
            "kind": "Lease",
            "metadata": metadata,
            "spec": {
                "holderIdentity": record.holder,
                "leaseDurationSeconds": record.duration_seconds,
                "acquireTime": record.acquire_time,
                "renewTime": record.renew_time,
                "leaseTransitions": record.transitions,
            }
        })
    }

    /// One request. Where it does not answer -- it failed to send, timed
    /// out, or was given up by its caller -- the client dials afresh for
    /// the next.
    async fn send(
        &self,
        request: impl FnOnce(&reqwest::Client) -> reqwest::RequestBuilder,
        what: &str,
    ) -> Result<(u16, String), String> {
        let token = self.token()?;
        let (made, http) = self.client();
        let mut unanswered = Unanswered(Some((self, made)));
        let response = request(&http)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|failed| format!("{what}: {failed}"))?;
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|failed| format!("{what}: {failed}"))?;
        unanswered.0 = None;
        Ok((status, body))
    }
}

/// Dials afresh, in place of the client its request went by, when dropped
/// before that request answered.
struct Unanswered<'a>(Option<(&'a KubeLeases, u64)>);

impl Drop for Unanswered<'_> {
    fn drop(&mut self) {
        if let Some((leases, made)) = self.0 {
            leases.dial_afresh(made);
        }
    }
}

/// A Lease's JSON as a [`Record`].
pub fn record_of(lease: &serde_json::Value) -> Record {
    let spec = &lease["spec"];
    let text = |value: &serde_json::Value| value.as_str().unwrap_or_default().to_string();
    Record {
        holder: text(&spec["holderIdentity"]),
        duration_seconds: spec["leaseDurationSeconds"].as_u64().unwrap_or(0),
        acquire_time: text(&spec["acquireTime"]),
        renew_time: text(&spec["renewTime"]),
        transitions: spec["leaseTransitions"].as_u64().unwrap_or(0),
        resource_version: text(&lease["metadata"]["resourceVersion"]),
        uid: text(&lease["metadata"]["uid"]),
    }
}

impl Leases for KubeLeases {
    async fn get(&self, name: &str) -> Result<Option<Record>, String> {
        let path = self.path(Some(name));
        let url = format!("{}{path}", self.base);
        let (status, body) = self
            .send(|http| http.get(url), &format!("GET {path}"))
            .await?;
        match status {
            404 => Ok(None),
            200..=299 => serde_json::from_str::<serde_json::Value>(&body)
                .map(|lease| Some(record_of(&lease)))
                .map_err(|failed| format!("GET {path}: {failed}")),
            _ => Err(format!("GET {path} -> {status}: {}", first_line(&body))),
        }
    }

    async fn create(&self, name: &str, record: &Record) -> Result<Wrote, String> {
        let path = self.path(None);
        let (url, body) = (
            format!("{}{path}", self.base),
            self.body(name, record).to_string(),
        );
        let (status, body) = self
            .send(
                |http| {
                    http.post(url)
                        .header("Content-Type", "application/json")
                        .body(body)
                },
                &format!("POST {path}"),
            )
            .await?;
        written(status, &body, &path)
    }

    async fn update(&self, name: &str, record: &Record) -> Result<Wrote, String> {
        let path = self.path(Some(name));
        let (url, body) = (
            format!("{}{path}", self.base),
            self.body(name, record).to_string(),
        );
        let (status, body) = self
            .send(
                |http| {
                    http.put(url)
                        .header("Content-Type", "application/json")
                        .body(body)
                },
                &format!("PUT {path}"),
            )
            .await?;
        written(status, &body, &path)
    }
}

fn written(status: u16, body: &str, path: &str) -> Result<Wrote, String> {
    match status {
        409 => Ok(Wrote::Conflict),
        200..=299 => {
            let lease: serde_json::Value =
                serde_json::from_str(body).map_err(|failed| format!("{path}: {failed}"))?;
            Ok(Wrote::Done(
                lease["metadata"]["resourceVersion"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            ))
        }
        _ => Err(format!("{path} -> {status}: {}", first_line(body))),
    }
}

fn first_line(body: &str) -> &str {
    body.lines().next().unwrap_or_default()
}

#[cfg(test)]
mod sim;
#[cfg(test)]
mod tests;
