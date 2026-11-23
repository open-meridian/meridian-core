//! A plugin instance's shards and its standby pod, by Lease (step 3d;
//! spec/a-deployment-scales-out-and-fails-over-by-configuration section 7;
//! rulings 1056 questions 11 and 12).
//!
//! An instance launched with `shards` above 1 or a `standby` runs several
//! pods of the one Deployment, each the same. There is one Lease per shard,
//! `<instance>-shard-<n>`, and each pod's sidecar takes the first that is
//! free or lapsed: the pod holding shard `n`'s is shard `n`. A pod that
//! finds every Lease held is the standby: its sidecar serves nothing -- no
//! plugin surface, no front door, so its plugin's registration waits and
//! its role classes' `run()` has not started -- and takes whichever Lease
//! lapses or is released, within the lease's duration.
//!
//! **The Leases are made for it** (step 3d, checkpoint 5, item 31): the
//! launcher, or the chart for a `sidecars[]` instance, makes each shard's
//! Lease before the instance's pods start and never deletes one, so its
//! `leaseTransitions` only grows. A sidecar takes and renews them under its
//! instance's own account, `<fullname>-shards-<instance>`, whose Role names
//! those Leases alone, and never makes one: a pod that finds its Lease
//! missing says so and waits.
//!
//! A shard reads the others' Leases, never writing them, to learn which
//! shards live: what the sidecar's rendezvous hashing shares the
//! instance's subjects over ([`meridian_sidecar::shards`]).
//!
//! **A data shard is fenced by its epoch** (step 3d, checkpoint 5, item
//! 32): taking a shard's Lease, its sidecar has the lake raise the shard's
//! epoch and stamps it on every write, and the lake and the book refuse a
//! write under an epoch lower than the one they hold. So a data shard, and
//! its standby, writes on through an API server outage, as a store's member
//! does: it stops only when a write is refused as fenced, or the Lease shows
//! another holder ([`ShardSet::writing_on`]).
//!
//! **Every other role stops by the clock**: the fence cannot stop what a
//! plugin does outside the stores -- an order path's venue send above all --
//! so a shard of any other role that cannot renew stops once it has gone
//! the lease's duration less one renewal unrenewed, before any standby can
//! see the lease lapse (client-go's renew deadline), and so does one that
//! finds another holder. Its serving ends, its plugin's stream with it, and
//! it stands by again in its process ([`serve_shards`]; item 49, ruled
//! 2026-11-16). Which stop applies is a property of the role, never a
//! setting.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::lease::{Elector, Leases, Settings, Standing};

/// The instance's shard count, set by the launcher on a launch with
/// several shards or a standby pod (1 for a standby alone). Unset: the
/// instance's one pod, which takes no Lease.
pub const SHARDS_ENV: &str = "MERIDIAN_PLUGIN_SHARDS";

/// The most shards an instance runs (`PluginDeclaration.most_shards`).
pub const MOST: u32 = 64;

/// Whether an instance of `roles` writes on through an API server outage,
/// fenced by its shard's epoch (step 3d, checkpoint 5, item 32): a data
/// plugin's, whose writes are the lake's rows. A property of the role,
/// never a setting.
pub fn writes_on(roles: &[String]) -> bool {
    !roles.is_empty() && roles.iter().all(|role| role == "data")
}

/// The Lease of an instance's shard `n`.
pub fn lease_name(instance: &str, shard: u32) -> String {
    format!("{instance}-shard-{shard}")
}

/// The shard count as the launcher said it: none where unsaid; refused
/// where it is not 1 to [`MOST`].
pub fn count(said: Option<&str>) -> Result<Option<u32>, String> {
    match said.map(str::trim).filter(|said| !said.is_empty()) {
        None => Ok(None),
        Some(text) => text
            .parse::<u32>()
            .ok()
            .filter(|count| (1..=MOST).contains(count))
            .map(Some)
            .ok_or_else(|| format!("{SHARDS_ENV} is {text:?}; a whole number, 1 to {MOST}")),
    }
}

/// Where a pod stands among its instance's shards after a look.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stand {
    /// Every shard's Lease is another pod's: the standby.
    Standby,
    /// This pod is shard `mine`; `others` are the other shards whose
    /// Leases are held.
    Shard { mine: u32, others: Vec<u32> },
    /// It held a shard and no longer may act as it: why.
    Lost(String),
}

/// How a shard's keeping ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kept {
    /// The pod was told to stop, and gave its Lease up.
    Released,
    /// It may no longer act as its shard: why.
    Lost(String),
}

/// The pod told to stop; never, where nothing is left to tell it.
pub async fn told(stop: &mut tokio::sync::watch::Receiver<bool>) {
    if stop.wait_for(|stop| *stop).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// The monotonic clock, which a test may pause.
fn clock() -> Instant {
    tokio::time::Instant::now().into_std()
}

/// How often a shard reads the others' Leases: every renewal, and less
/// often past 17 shards, so one shard sends the API server at most eight
/// reads a second however many shards its instance runs.
fn read_every(renew: Duration, count: usize) -> Duration {
    renew.max(Duration::from_millis(125) * count.saturating_sub(1) as u32)
}

type Shared<L> = Arc<tokio::sync::Mutex<Elector<Arc<L>>>>;

/// One pod's view of its instance's shard Leases.
pub struct ShardSet<L: Leases> {
    api: Arc<L>,
    instance: String,
    electors: Vec<Shared<L>>,
    settings: Settings,
    mine: Option<usize>,
    /// Which other shards were live when each was last read.
    live: Vec<bool>,
    /// Whether its shard writes on unrenewed, fenced by its epoch
    /// ([`ShardSet::writing_on`]); else it stops at its renew deadline.
    writes_on: bool,
    /// When it last renewed its own Lease, shared with its sends' gate.
    renewal: Renewal,
}

/// When a pod last renewed its shard's Lease, shared with the gate on what
/// its sidecar sends (review c5a M1, ruled 2026-11-16): a data shard writes
/// on through an API server outage with its lake record writes alone, the
/// writes its epoch fences; every other send waits for a renewal once the
/// Lease has gone its renew deadline unrenewed.
#[derive(Debug, Clone)]
pub struct Renewal {
    at: Arc<std::sync::Mutex<Option<Instant>>>,
    deadline: Duration,
}

impl Renewal {
    fn new(deadline: Duration) -> Self {
        Self {
            at: Arc::default(),
            deadline,
        }
    }

    fn renewed(&self, at: Instant) {
        let mut held = self.at.lock().expect("renewal poisoned");
        *held = Some(held.map_or(at, |was| was.max(at)));
    }

    /// Whether the Lease was renewed within its renew deadline: true before
    /// any take, which nothing is gated by.
    pub fn within_deadline(&self) -> bool {
        self.at
            .lock()
            .expect("renewal poisoned")
            .is_none_or(|at| clock().saturating_duration_since(at) < self.deadline)
    }
}

/// Whether `topic` is sent through an API server outage, past the Lease's
/// renew deadline: a lake record write -- the writes a shard's epoch fences
/// -- its write-ahead stream's subject, or a read, which changes nothing.
pub fn sent_on_unrenewed(topic: &str) -> bool {
    meridian_lake::service::is_record_write(topic)
        || meridian_bus::write_ahead::dataset_of_subject(topic).is_some()
        || topic.split('.').nth(2) == Some("query")
}

/// The gate on what a data shard's sidecar sends (review c5a M1): every
/// send but [`sent_on_unrenewed`]'s held back while `renewal` is past its
/// renew deadline.
pub fn send_gate(renewal: Renewal) -> meridian_bus::SendGate {
    Arc::new(move |topic: &str| {
        if renewal.within_deadline() || sent_on_unrenewed(topic) {
            Ok(())
        } else {
            Err(format!(
                "this shard's Lease has gone its renew deadline ({} s) unrenewed: it sends its lake \
                 record writes alone until it renews",
                renewal.deadline.as_secs()
            ))
        }
    })
}

/// The gate on what a shard's sidecar sends (review c5a6 L3): nothing at
/// all once its serving has `ended` -- set as a lost serving ends, before
/// its runtime is shut, so nothing it still holds sends in between -- and
/// for a data shard, given its `renewal`, [`send_gate`]'s.
pub fn serving_gate(
    ended: Arc<std::sync::atomic::AtomicBool>,
    renewal: Option<Renewal>,
) -> meridian_bus::SendGate {
    let renewed = renewal.map(send_gate);
    Arc::new(move |topic: &str| {
        if ended.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("this shard's serving has ended: it sends nothing more".into());
        }
        renewed.as_ref().map_or(Ok(()), |gate| gate(topic))
    })
}

/// The Leases of `shards` read at once, each by looking alone: whether
/// each is held, and none where it could not be read.
async fn read<L: Leases + 'static>(
    shards: &[(usize, Shared<L>)],
    now: Instant,
) -> Vec<(usize, Option<bool>)> {
    let mut reads = tokio::task::JoinSet::new();
    for (shard, elector) in shards {
        let (shard, elector) = (*shard, Arc::clone(elector));
        reads.spawn(async move {
            let held = elector.lock().await.watch(now).await;
            (shard, held)
        });
    }
    let mut read = Vec::with_capacity(shards.len());
    while let Some(joined) = reads.join_next().await {
        let Ok((shard, held)) = joined else { continue };
        read.push((
            shard,
            match held {
                Ok(holder) => Some(holder.is_some()),
                Err(failed) => {
                    tracing::debug!(shard, %failed, "another shard's lease could not be read");
                    None
                }
            },
        ));
    }
    read
}

impl<L: Leases + 'static> ShardSet<L> {
    pub fn new(
        api: Arc<L>,
        instance: &str,
        count: u32,
        holder: &str,
        renew: Duration,
        duration: Duration,
    ) -> Result<Self, String> {
        let settings = Settings {
            name: lease_name(instance, 0),
            holder: holder.to_string(),
            renew,
            duration,
        };
        settings.check()?;
        let mut set = Self {
            api,
            instance: instance.to_string(),
            electors: Vec::new(),
            renewal: Renewal::new(settings.renew_deadline()),
            settings,
            mine: None,
            live: vec![false; count.max(1) as usize],
            writes_on: false,
        };
        set.electors = set.electors_of(count.max(1));
        Ok(set)
    }

    /// One elector per shard's Lease, each holding a record seen, and a
    /// lapse judged, by when its read answered. A holder that stops by the
    /// clock has its standbys count a stretch they could not read toward
    /// its lapse; one that writes on, never ([`ShardSet::writing_on`]).
    fn electors_of(&self, count: u32) -> Vec<Shared<L>> {
        (0..count)
            .map(|shard| {
                // Never made here (item 31): the launcher, or the chart,
                // makes each shard's Lease before the instance's pods start.
                let elector = Elector::new(
                    Arc::clone(&self.api),
                    Settings {
                        name: lease_name(&self.instance, shard),
                        ..self.settings.clone()
                    },
                )
                .makes_none();
                Arc::new(tokio::sync::Mutex::new(if self.writes_on {
                    elector.seen_as_answered(clock)
                } else {
                    elector.of_a_holder_that_stops(clock)
                }))
            })
            .collect()
    }

    /// A shard fenced by its epoch, which writes on through an API server
    /// outage (step 3d, checkpoint 5, item 32; [`writes_on`]): a renewal
    /// that fails or hangs never ends it. It is lost only when its Lease
    /// shows another holder -- or, outside this set, when the lake or the
    /// book refuses one of its writes as fenced.
    ///
    /// **Its standby, likewise, counts only what it observed** (review c5a5
    /// F4; item 32's Rule), as a store's member does: a stretch in which
    /// it could not read a Lease is never its holder's silence, since that
    /// holder wrote on through it. Once the API server answers again, the
    /// standby waits a whole lapse from its first read before it takes --
    /// and so one read of the Lease failing while it waits out a lapse
    /// costs it a whole lapse again (PO item 54, kept under A): its 10 s
    /// takeover holds where its reads answer. The Lease it takes is then kept as seen through a failed look, so
    /// its take and its release never miss for one API error (review c5a6
    /// M1). Said before the set's first look.
    pub fn writing_on(mut self) -> Self {
        self.writes_on = true;
        self.electors = self.electors_of(self.electors.len() as u32);
        self
    }

    /// The shard this pod holds, the Lease's transitions as it took it --
    /// the floor of the epoch the lake raises (item 32) -- whom it took it
    /// from, this pod's holder name, and whether the Lease was free (released
    /// or never held). None before a take.
    pub async fn taken(&self) -> Option<Taken> {
        let mine = self.mine?;
        let elector = self.electors[mine].lock().await;
        Some(Taken {
            shard: mine as u32,
            transitions: elector.transitions()?,
            from: elector.took_from().flatten().unwrap_or_default(),
            holder: elector.settings().holder.clone(),
            free: elector.took_free(),
            lease_uid: elector.uid().unwrap_or_default(),
        })
    }

    /// When this pod last renewed its Lease, for the gate on what it sends
    /// ([`send_gate`]).
    pub fn renewal(&self) -> Renewal {
        self.renewal.clone()
    }

    /// The Lease's duration: how long a holder's take may stand unrenewed
    /// before a standby may take it.
    pub fn lasts(&self) -> Duration {
        self.settings.duration
    }

    /// How often to look, from one look's start to the next: a standby
    /// twice a renewal (every 1 s at the chart's 2 s; PO item 54), as a
    /// store's does; a shard every renewal.
    pub fn look_every(&self) -> Duration {
        match self.mine {
            None => self.settings.look_every(),
            Some(_) => self.settings.renew,
        }
    }

    fn others(&self, mine: usize) -> Vec<(usize, Shared<L>)> {
        self.electors
            .iter()
            .enumerate()
            .filter(|(shard, _)| *shard != mine)
            .map(|(shard, elector)| (shard, Arc::clone(elector)))
            .collect()
    }

    /// One look. A pod holding no shard takes the first Lease free or
    /// lapsed, and reads the others' in the same look: it never stands as
    /// its instance's only live shard, and so every subject's keeper, until
    /// its next. A shard renews its own and reads the others'.
    pub async fn look(&mut self, now: Instant, wall_ns: i64) -> Stand {
        let Some(mine) = self.mine else {
            let mut taken = None;
            for (shard, elector) in self.electors.iter().enumerate() {
                match elector.lock().await.look(now, wall_ns).await {
                    Ok(Standing::Held) => {
                        taken = Some(shard);
                        break;
                    }
                    Ok(Standing::HeldBy(_)) => {}
                    Err(failed) => {
                        tracing::warn!(shard, %failed, "a shard's lease could not be read");
                    }
                }
            }
            let Some(mine) = taken else {
                return Stand::Standby;
            };
            self.mine = Some(mine);
            // Its own Lease kept as seen through a failed look, as every
            // holder's is: only a data standby's lapse count drops it
            // (review c5a6 M1; F4).
            if self.writes_on {
                self.electors[mine]
                    .lock()
                    .await
                    .keep_seen_through_failures(clock);
            }
            // One not read is held live, as the others of a running
            // instance are, until it is read.
            for (shard, held) in read(&self.others(mine), now).await {
                self.live[shard] = held.unwrap_or(true);
            }
            return self.standing(mine);
        };
        let looked = self.electors[mine].lock().await.look(now, wall_ns).await;
        match looked {
            Ok(Standing::Held) => {
                if let Some(at) = self.electors[mine].lock().await.renewed_at() {
                    self.renewal.renewed(at);
                }
            }
            Ok(Standing::HeldBy(other)) => return Stand::Lost(self.held_by(&other)),
            Err(failed) => {
                let deadline = self.settings.renew_deadline();
                let since = self.electors[mine]
                    .lock()
                    .await
                    .renewed_at()
                    .map_or(deadline, |at| now.saturating_duration_since(at));
                if since >= deadline && !self.writes_on {
                    return Stand::Lost(format!("{}: {failed}", self.unrenewed(mine)));
                }
                tracing::warn!(shard = mine, %failed, "the shard's lease was not renewed; trying again");
            }
        }
        for (shard, held) in read(&self.others(mine), now).await {
            // Not read: as it was last read.
            if let Some(held) = held {
                self.live[shard] = held;
            }
        }
        self.standing(mine)
    }

    fn held_by(&self, other: &str) -> String {
        format!(
            "{} is held by {}",
            self.mine
                .map(|mine| lease_name(&self.instance, mine as u32))
                .unwrap_or_default(),
            if other.is_empty() { "nobody" } else { other }
        )
    }

    fn unrenewed(&self, mine: usize) -> String {
        format!(
            "{} could not be renewed for {} s, the lease's duration less one renewal",
            lease_name(&self.instance, mine as u32),
            self.settings.renew_deadline().as_secs()
        )
    }

    fn standing(&self, mine: usize) -> Stand {
        Stand::Shard {
            mine: mine as u32,
            others: self.live_others(mine),
        }
    }

    fn live_others(&self, mine: usize) -> Vec<u32> {
        (0..self.electors.len())
            .filter(|shard| *shard != mine && self.live[*shard])
            .map(|shard| shard as u32)
            .collect()
    }

    /// Wait until one of the instance's shard Leases is this pod's: at
    /// once where one is free, else as its standby until one lapses or is
    /// released. None where the pod was told to stop while it waited: it
    /// holds no Lease then, a look in flight when it was told given one
    /// renewal to end and what it took released.
    pub async fn take(
        mut self,
        mut stop: tokio::sync::watch::Receiver<bool>,
    ) -> Result<Option<(Self, u32, Vec<u32>)>, String> {
        let mut said = false;
        loop {
            // Looks start a look apart, however long one took (PO item 54):
            // a slow read never stretches a standby's cadence.
            let started = tokio::time::Instant::now();
            let renew = self.settings.renew;
            let stand = {
                let look = self.look(clock(), crate::now_ns());
                tokio::pin!(look);
                tokio::select! {
                    biased;
                    stand = &mut look => stand,
                    _ = told(&mut stop) => tokio::time::timeout(renew, &mut look)
                        .await
                        .unwrap_or(Stand::Standby),
                }
            };
            if *stop.borrow() {
                self.release(crate::now_ns()).await;
                return Ok(None);
            }
            match stand {
                Stand::Shard { mine, others } => return Ok(Some((self, mine, others))),
                Stand::Standby if !said => {
                    said = true;
                    tracing::info!(
                        instance = self.instance,
                        shards = self.electors.len(),
                        "every shard's lease is held: standing by, serving nothing until one is this pod's"
                    );
                }
                Stand::Standby => {}
                Stand::Lost(why) => return Err(why),
            }
            tokio::select! {
                _ = tokio::time::sleep_until(started + self.look_every()) => {}
                _ = told(&mut stop) => return Ok(None),
            }
        }
    }

    /// Its shard kept until the pod is told to stop, when it is released,
    /// or it is lost. `live` is told the other live shards at each read.
    ///
    /// **Its own Lease is renewed on its own cadence**, whatever the
    /// reading of the others takes: they are read apart, at once, and a
    /// slow read delays no renewal. **It is lost at its renew deadline by
    /// the clock**, a renewal in flight or not: a request that hangs is
    /// not waited for past the moment a standby may begin to count the
    /// lease lapsed -- unless it writes on ([`ShardSet::writing_on`]), when
    /// a renewal that hangs is given up after one renewal and tried again,
    /// and only another holder ends it.
    pub async fn keep(
        self,
        live: impl Fn(Vec<u32>) + Send,
        mut stop: tokio::sync::watch::Receiver<bool>,
    ) -> Kept {
        let Some(mine) = self.mine else {
            return Kept::Lost(format!("{} holds no shard to keep", self.instance));
        };
        let own = Arc::clone(&self.electors[mine]);
        let (renew, retry, deadline) = (
            self.settings.renew,
            self.settings.look_every(),
            self.settings.renew_deadline(),
        );
        // The others read apart, and their reading ended with this.
        let (tell, mut read_live) = tokio::sync::watch::channel(self.live_others(mine));
        let reading = Reading(tokio::spawn({
            let others = self.others(mine);
            let every = read_every(renew, self.electors.len());
            let mut live = self.live.clone();
            async move {
                loop {
                    tokio::time::sleep(every).await;
                    for (shard, held) in read(&others, clock()).await {
                        if let Some(held) = held {
                            live[shard] = held;
                        }
                    }
                    let now: Vec<u32> = (0..live.len())
                        .filter(|shard| *shard != mine && live[*shard])
                        .map(|shard| shard as u32)
                        .collect();
                    if tell.send(now).is_err() {
                        return;
                    }
                }
            }
        }));
        let mut renewing = tokio::time::interval_at(tokio::time::Instant::now() + renew, renew);
        renewing.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let writes_on = self.writes_on;
        let kept = loop {
            let Some(renewed) = own.lock().await.renewed_at() else {
                break Kept::Lost(self.held_by("another pod"));
            };
            self.renewal.renewed(renewed);
            let stop_by = tokio::time::Instant::from_std(renewed + deadline);
            let deadline_passed = async {
                if writes_on {
                    std::future::pending::<()>().await
                } else {
                    tokio::time::sleep_until(stop_by).await
                }
            };
            tokio::select! {
                biased;
                _ = told(&mut stop) => {
                    own.lock().await.release(crate::now_ns()).await;
                    break Kept::Released;
                }
                _ = deadline_passed => break Kept::Lost(self.unrenewed(mine)),
                _ = renewing.tick() => {
                    let looking = async { own.lock().await.look(clock(), crate::now_ns()).await };
                    // Fenced, it writes on: a renewal waits one renewal at
                    // most from its tick -- never from before it (review
                    // c5a5 F5) -- and is tried again; the clock never ends it.
                    let give_up_by = if writes_on {
                        tokio::time::Instant::now() + renew
                    } else {
                        stop_by
                    };
                    match tokio::time::timeout_at(give_up_by, looking).await {
                        Err(_) if writes_on => {
                            tracing::warn!(shard = mine, "the shard's lease renewal did not answer; writing on, fenced by its epoch, and trying again");
                            renewing.reset_after(retry);
                        }
                        Err(_) => break Kept::Lost(self.unrenewed(mine)),
                        Ok(Ok(Standing::Held)) => {}
                        Ok(Ok(Standing::HeldBy(other))) => break Kept::Lost(self.held_by(&other)),
                        Ok(Err(failed)) => {
                            tracing::warn!(shard = mine, %failed, "the shard's lease was not renewed; trying again");
                            renewing.reset_after(retry);
                        }
                    }
                }
                Ok(()) = read_live.changed() => live(read_live.borrow_and_update().clone()),
            }
        };
        drop(reading);
        kept
    }

    /// Its own Lease renewed now, and the others read: what a pod does once
    /// its take's epoch is raised (item 32), so the time the raise took is
    /// not counted toward its lease's lapse before its keeping begins.
    pub async fn renew_now(&mut self) -> Result<(), String> {
        match self.look(clock(), crate::now_ns()).await {
            Stand::Shard { .. } => Ok(()),
            Stand::Lost(why) => Err(why),
            Stand::Standby => Err("this pod holds no shard".into()),
        }
    }

    /// This pod's own Lease, held apart from its keeping, to give up as a
    /// serving ends unplanned ([`Holding::give_up`]): none before a take.
    pub fn holding(&self) -> Option<Holding<L>> {
        self.mine
            .map(|mine| Holding(Arc::clone(&self.electors[mine])))
    }

    /// Give this pod's shard up: a planned stop hands it to the standby at
    /// once.
    pub async fn release(&mut self, wall_ns: i64) {
        if let Some(mine) = self.mine {
            self.electors[mine].lock().await.release(wall_ns).await;
        }
    }
}

/// A pod's own shard Lease, held apart from its keeping (review c5a6 L4).
pub struct Holding<L: Leases>(Shared<L>);

impl<L: Leases + 'static> Holding<L> {
    /// Given up where it still names this pod, by a write conditional on a
    /// fresh read: a serving that ended other than by a planned stop -- a
    /// write refused as fenced, its surface or front door stopped -- hands
    /// its shard on at once, not after a lapse under a name that is gone. A
    /// Lease another pod took is never touched.
    pub async fn give_up(&self) {
        match self.0.lock().await.release_if_mine(crate::now_ns()).await {
            Ok(true) => tracing::info!("the shard's lease is released"),
            Ok(false) => {}
            Err(failed) => {
                tracing::warn!(%failed, "the shard's lease could not be released; it lapses on its own")
            }
        }
    }
}

/// How a pod's serving of the shard it took ended ([`serve_shards`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Served {
    /// Told to stop: its Lease given up, and the pod stops.
    Stopped,
    /// It may no longer serve its shard -- the Lease shows another holder,
    /// or a store refused a write as fenced: why. It stands by again.
    Lost(String),
    /// Its take did not complete -- its epoch not raised, or its bus not
    /// set up inside the Lease -- and its Lease was given up: why. It
    /// stands by again once a lapse has passed.
    NotTaken(String),
}

/// A pod's shards, served until it is told to stop (step 3d, checkpoint 5,
/// item 32 as item 49 ruled it, 2026-11-16): it stands by, takes a shard,
/// and serves it with `serve`. When it loses that shard -- another holder
/// on its Lease, or a write refused as fenced -- or its take does not
/// complete, **it stands by again in the same process**, under a new holder
/// name from `set`. It never ends its process to do so: a container that
/// ends is started again only after the kubelet's back-off, 10 s doubling
/// to 5 min, and its pod is no standby for that long (review c5a5 F3).
///
/// A take that did not complete waits a lapse before it stands by again,
/// so a store not yet up is not asked again and again, and another standby
/// may take the shard meanwhile. An error -- from `set`, or from `serve`
/// for a fault of the pod's own -- ends it.
pub async fn serve_shards<L, M, S, F>(
    mut set: M,
    mut serve: S,
    stop: tokio::sync::watch::Receiver<bool>,
) -> Result<(), String>
where
    L: Leases + 'static,
    M: FnMut() -> Result<ShardSet<L>, String>,
    S: FnMut(ShardSet<L>, u32, Vec<u32>) -> F,
    F: std::future::Future<Output = Result<Served, String>>,
{
    loop {
        let standing = set()?;
        let lasts = standing.lasts();
        let Some((taken, mine, others)) = standing.take(stop.clone()).await? else {
            return Ok(());
        };
        match serve(taken, mine, others).await? {
            Served::Stopped => return Ok(()),
            Served::Lost(why) => {
                tracing::warn!(shard = mine, "{why}; standing by again");
            }
            Served::NotTaken(why) => {
                tracing::warn!(
                    shard = mine,
                    "{why}; its lease given up, standing by again after a lapse"
                );
                let mut stop = stop.clone();
                tokio::select! {
                    _ = tokio::time::sleep(lasts) => {}
                    _ = told(&mut stop) => return Ok(()),
                }
            }
        }
        if *stop.borrow() {
            return Ok(());
        }
    }
}

/// A shard's take as this pod made it ([`ShardSet::taken`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Taken {
    pub shard: u32,
    /// The Lease's transitions as it took it.
    pub transitions: u64,
    /// Whom it took it from: empty where it never saw a holder.
    pub from: String,
    /// This pod's holder name.
    pub holder: String,
    /// Whether the Lease was free: released, or never held.
    pub free: bool,
    /// The Lease object's uid as it read it: the lake keeps the floor of
    /// transitions on it (review c5a2 N5).
    pub lease_uid: String,
}

/// How long a take waits for the lake: the lake's confirm window and the
/// way there and back, one constant shared with the lake (review c5a M3).
/// The Lease is renewed just before it and just after it
/// ([`take_epoch`]), so the take ends inside the renew deadline.
pub const TAKE_SHARD_WITHIN: Duration = meridian_changes::shard_fence::TAKE_WITHIN;

/// The shard `set` just took, its epoch raised (step 3d, checkpoint 5,
/// item 32): the Lease renewed first -- read again, so a pod whose take
/// stalled past its lapse finds the holder that took it since and never
/// asks (review c5a M2) -- then [`raise_epoch`], then renewed again before
/// anything is served.
pub async fn take_epoch<L: Leases + 'static>(
    bus: &meridian_bus::Bus,
    instance: &str,
    set: &mut ShardSet<L>,
) -> Result<u64, String> {
    set.renew_now().await?;
    let epoch = raise_epoch(bus, instance, set).await?;
    set.renew_now().await?;
    Ok(epoch)
}

/// The shard `set` just took has the lake raise its epoch (step 3d,
/// checkpoint 5, item 32): `TakeShard`, answered once every lake and book
/// writer group has raised it, and `bus` stamps it on every message from
/// here on (`MessageMeta.shard`, `.shard_epoch`). Answers the epoch.
pub async fn raise_epoch<L: Leases + 'static>(
    bus: &meridian_bus::Bus,
    instance: &str,
    set: &ShardSet<L>,
) -> Result<u64, String> {
    use prost::Message as _;
    let Taken {
        shard,
        transitions,
        from: previous_holder,
        holder,
        free,
        lease_uid,
    } = set
        .taken()
        .await
        .ok_or("this pod holds no shard to raise the epoch of")?;
    if lease_uid.is_empty() {
        return Err("the shard's Lease was taken unread: its uid is not known".into());
    }
    let reason = if free {
        meridian_domain::v1::TakeoverReason::Released
    } else {
        meridian_domain::v1::TakeoverReason::LeaseLapsed
    };
    let request = meridian_domain::v1::TakeShardRequest {
        instance: instance.to_string(),
        shard,
        lease_transitions: transitions,
        holder,
        previous_holder,
        reason: reason as i32,
        lease_uid,
    };
    let (_, body) = bus
        .call(
            meridian_lake::service::TAKE_SHARD,
            "meridian.v1.TakeShardRequest",
            request.encode_to_vec(),
            None,
            Some(TAKE_SHARD_WITHIN),
        )
        .await
        .map_err(|failed| failed.to_string())?;
    let epoch = meridian_domain::v1::TakeShardReply::decode(body.as_slice())
        .map_err(|failed| failed.to_string())?
        .epoch;
    if epoch == 0 {
        return Err("the lake answered epoch 0".into());
    }
    bus.stamp_shard(shard, epoch)?;
    tracing::info!(
        instance,
        shard,
        epoch,
        "the shard's epoch is raised in every writer group: stamped on every write"
    );
    Ok(epoch)
}

/// The book's fences raised to the lake's records, asked again every few
/// seconds until it is done or `within` has passed: as a book writer
/// starts, and on a new database timeline (review c5a H1). Best effort: a
/// backstop, since every book raise commits on the synchronous standby
/// (review c5a3 M2). Said once raised, once failing, and once given up.
/// Answers whether it was done.
pub async fn raise_book_until_done<F>(
    bus: Arc<meridian_bus::Bus>,
    raise: F,
    within: Duration,
) -> bool
where
    F: Fn(&str, u32, u64) -> Result<u64, String> + Clone + Send + Sync + 'static,
{
    let raising = async {
        let mut said_failing = false;
        loop {
            match raise_book_to_records(&bus, raise.clone()).await {
                Ok(shards) => {
                    tracing::info!(
                        shards,
                        "the book's plugin shard fences raised to the lake's records of their takes"
                    );
                    return;
                }
                Err(failed) if !said_failing => {
                    tracing::warn!(%failed, "the book's plugin shard fences were not raised to the lake's records; asking again");
                    said_failing = true;
                }
                Err(_) => {}
            }
            tokio::time::sleep(RECORDS_PAGE_WITHIN).await;
        }
    };
    let done = tokio::time::timeout(within, raising).await.is_ok();
    if !done {
        tracing::warn!(
            within_s = within.as_secs(),
            "the book's plugin shard fences were not raised to the lake's records in time; \
             serving on: each take raises every book group before it is answered"
        );
    }
    done
}

/// The book's fences raised to the lake's records again each time a store
/// of this process raises its epoch on a new database timeline, whichever
/// connection met it first (review c5a2 N3): looked at every second.
pub async fn raise_book_on_new_timelines<F>(bus: Arc<meridian_bus::Bus>, raise: F)
where
    F: Fn(&str, u32, u64) -> Result<u64, String> + Clone + Send + Sync + 'static,
{
    let mut seen = meridian_changes::timeline::raised();
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let now = meridian_changes::timeline::raised();
        if now != seen {
            seen = now;
            raise_book_until_done(Arc::clone(&bus), raise.clone(), RECORDS_WITHIN).await;
        }
    }
}

/// How long the whole read of the lake's records of the takes may take
/// (review c5a2 N6), and how long a book asks again for it (c5a3 M2).
pub const RECORDS_WITHIN: Duration = Duration::from_secs(30);

/// How long one page of the lake's control partition is waited for, read
/// for its shards' takes.
const RECORDS_PAGE_WITHIN: Duration = Duration::from_secs(5);

/// Each plugin shard's highest epoch its takes' records name, read from
/// the lake's control partition through its read of changes (review c5a
/// H1): what the book raises its fence rows to, since the book records no
/// take of its own.
pub async fn recorded_epochs(
    bus: &meridian_bus::Bus,
) -> Result<std::collections::BTreeMap<(String, u32), u64>, String> {
    use prost::Message as _;
    let mut bodies: Vec<Vec<u8>> = Vec::new();
    let mut after = 0;
    loop {
        let asked = meridian_changes::ReadChangesRequest {
            partition: meridian_changes::CONTROL.to_string(),
            after_sequence: after,
            page_size: 0,
        };
        let (_, body) = bus
            .call(
                &meridian_changes::read_changes_topic("lake"),
                "meridian.v1.ReadChangesRequest",
                asked.encode_to_vec(),
                None,
                Some(RECORDS_PAGE_WITHIN),
            )
            .await
            .map_err(|failed| format!("the lake's control partition was not read: {failed}"))?;
        let page = meridian_changes::ReadChangesReply::decode(body.as_slice())
            .map_err(|failed| failed.to_string())?;
        for change in &page.changes {
            after = change.sequence;
            if change.kind == meridian_lake::store::CONTROL_SHARD_TOOK_OVER {
                bodies.push(change.body.clone());
            }
        }
        if page.changes.is_empty() || after >= page.head {
            break;
        }
    }
    Ok(meridian_changes::shard_fence::highest_recorded(
        bodies.iter().map(Vec::as_slice),
    ))
}

/// The book's plugin shard fences raised to the lake's records (review c5a
/// H1, L1): as a book writer starts, before it serves, and on a new
/// database timeline, which may have lost a take's raise. `raise` is the
/// book store's own, in every database its group writes. Answers how many
/// shards the records name.
pub async fn raise_book_to_records<F>(bus: &meridian_bus::Bus, raise: F) -> Result<usize, String>
where
    F: Fn(&str, u32, u64) -> Result<u64, String> + Send + Sync + 'static,
{
    let recorded = tokio::time::timeout(RECORDS_WITHIN, recorded_epochs(bus))
        .await
        .map_err(|_| {
            format!(
                "the lake's records of the takes were not read within {} s",
                RECORDS_WITHIN.as_secs()
            )
        })??;
    tokio::task::spawn_blocking(move || {
        for ((instance, shard), epoch) in &recorded {
            raise(instance, *shard, *epoch)?;
        }
        Ok(recorded.len())
    })
    .await
    .map_err(|failed| failed.to_string())?
}

/// The reading of the other shards' Leases, ended when it is dropped.
struct Reading(tokio::task::JoinHandle<()>);

impl Drop for Reading {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::lease::{Record, Wrote};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    /// The namespace's Leases, as the API server keeps them: every write
    /// conditional on the version read. `down` answers nothing.
    #[derive(Default)]
    pub(crate) struct Memory {
        pub(crate) leases: Mutex<HashMap<String, Record>>,
        pub(crate) down: AtomicBool,
        /// When each Lease was last written.
        written: Mutex<HashMap<String, tokio::time::Instant>>,
        /// How many Leases were made: each made one's uid.
        made: std::sync::atomic::AtomicU64,
    }

    impl Memory {
        fn up(&self) -> Result<(), String> {
            if self.down.load(Ordering::SeqCst) {
                Err("the API server did not answer".into())
            } else {
                Ok(())
            }
        }
        fn wrote(&self, name: &str) {
            self.written
                .lock()
                .unwrap()
                .insert(name.to_string(), tokio::time::Instant::now());
        }
        fn written_at(&self, name: &str) -> tokio::time::Instant {
            self.written.lock().unwrap()[name]
        }
        /// A Lease made free, as the launcher or the chart makes each shard's
        /// before its pods start (item 31); one already there is kept.
        pub(crate) fn make(&self, name: &str) {
            let mut leases = self.leases.lock().unwrap();
            if leases.contains_key(name) {
                return;
            }
            let uid = format!("{name}@{}", self.made.fetch_add(1, Ordering::SeqCst));
            leases.insert(
                name.to_string(),
                Record {
                    holder: String::new(),
                    duration_seconds: 0,
                    acquire_time: String::new(),
                    renew_time: String::new(),
                    transitions: 0,
                    resource_version: "1".into(),
                    uid,
                    metadata: serde_json::Value::Null,
                },
            );
            drop(leases);
            self.wrote(name);
        }
        /// The instance's `count` shard Leases, made.
        pub(crate) fn make_shards(&self, count: u32) {
            for shard in 0..count.max(1) {
                self.make(&lease_name("feed-1", shard));
            }
        }
        pub(crate) fn holder(&self, name: &str) -> String {
            self.leases
                .lock()
                .unwrap()
                .get(name)
                .map(|record| record.holder.clone())
                .unwrap_or_default()
        }
    }

    impl Leases for Memory {
        async fn get(&self, name: &str) -> Result<Option<Record>, String> {
            self.up()?;
            Ok(self.leases.lock().unwrap().get(name).cloned())
        }
        async fn create(&self, name: &str, record: &Record) -> Result<Wrote, String> {
            self.up()?;
            let mut leases = self.leases.lock().unwrap();
            if leases.contains_key(name) {
                return Ok(Wrote::Conflict);
            }
            let mut record = record.clone();
            record.resource_version = "1".into();
            // The API server's: a Lease made again has another.
            record.uid = format!("{name}@{}", self.made.fetch_add(1, Ordering::SeqCst));
            leases.insert(name.to_string(), record);
            self.wrote(name);
            Ok(Wrote::Done("1".into()))
        }
        async fn update(&self, name: &str, record: &Record) -> Result<Wrote, String> {
            self.up()?;
            let mut leases = self.leases.lock().unwrap();
            let Some(current) = leases.get(name) else {
                return Ok(Wrote::Conflict);
            };
            if current.resource_version != record.resource_version {
                return Ok(Wrote::Conflict);
            }
            let next = (current.resource_version.parse::<u64>().unwrap() + 1).to_string();
            let mut record = record.clone();
            record.resource_version = next.clone();
            record.uid = current.uid.clone();
            leases.insert(name.to_string(), record);
            self.wrote(name);
            Ok(Wrote::Done(next))
        }
    }

    const RENEW: Duration = Duration::from_secs(2);
    const LASTS: Duration = Duration::from_secs(6);

    fn pod(api: &Arc<Memory>, count: u32, holder: &str) -> ShardSet<Memory> {
        api.make_shards(count);
        ShardSet::new(Arc::clone(api), "feed-1", count, holder, RENEW, LASTS).unwrap()
    }

    fn run<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    fn shard(mine: u32, others: &[u32]) -> Stand {
        Stand::Shard {
            mine,
            others: others.to_vec(),
        }
    }

    #[test]
    fn the_count_is_one_to_sixty_four_and_unsaid_is_no_lease() {
        assert_eq!(count(None), Ok(None));
        assert_eq!(count(Some("")), Ok(None));
        assert_eq!(count(Some("1")), Ok(Some(1)));
        assert_eq!(count(Some("64")), Ok(Some(64)));
        for bad in ["0", "65", "two", "-1"] {
            assert!(count(Some(bad)).unwrap_err().contains(SHARDS_ENV), "{bad}");
        }
        assert_eq!(lease_name("feed-1", 3), "feed-1-shard-3");
    }

    /// Item 31: a shard's Lease is the launcher's or the chart's to make.
    /// A pod that finds none takes nothing and makes nothing -- its sidecar
    /// holds no right to -- and stands by until it is made.
    #[test]
    fn a_pod_never_makes_its_shard_s_lease_and_waits_for_it() {
        run(async {
            let api = Arc::new(Memory::default());
            let mut a = ShardSet::new(Arc::clone(&api), "feed-1", 2, "a", RENEW, LASTS).unwrap();
            assert_eq!(a.look(Instant::now(), 0).await, Stand::Standby);
            assert!(api.leases.lock().unwrap().is_empty(), "no Lease made");
            assert_eq!(a.taken().await, None);
            api.make("feed-1-shard-1");
            assert_eq!(a.look(Instant::now(), 0).await, shard(1, &[]));
            assert_eq!(api.holder("feed-1-shard-1"), "a");
            assert!(!api.leases.lock().unwrap().contains_key("feed-1-shard-0"));
        });
    }

    #[test]
    fn three_pods_of_two_shards_are_each_shard_once_and_the_third_stands_by() {
        run(async {
            let api = Arc::new(Memory::default());
            let (mut a, mut b, mut c) = (pod(&api, 2, "a"), pod(&api, 2, "b"), pod(&api, 2, "c"));
            let start = Instant::now();
            assert_eq!(a.look(start, 0).await, shard(0, &[]));
            // A pod that takes a shard reads the others in the same look.
            assert_eq!(b.look(start, 0).await, shard(1, &[0]));
            assert_eq!(c.look(start, 0).await, Stand::Standby);
            // Each shard reads the other live at its next look.
            let next = start + RENEW;
            assert_eq!(a.look(next, 0).await, shard(0, &[1]));
            assert_eq!(b.look(next, 0).await, shard(1, &[0]));
            assert_eq!(c.look(next, 0).await, Stand::Standby);
            assert_eq!(api.holder("feed-1-shard-0"), "a");
            assert_eq!(api.holder("feed-1-shard-1"), "b");
        });
    }

    #[test]
    fn a_shard_that_stops_renewing_is_the_standby_s_within_the_lease_and_the_other_reads_it() {
        run(async {
            let api = Arc::new(Memory::default());
            let (mut a, mut b, mut c) = (pod(&api, 2, "a"), pod(&api, 2, "b"), pod(&api, 2, "c"));
            let start = Instant::now();
            a.look(start, 0).await;
            b.look(start, 0).await;
            c.look(start, 0).await;
            // Shard 1's pod is gone: `b` looks no more. `a` renews; `c`
            // watches, and takes shard 1 once it has seen it unrenewed for
            // the lease's duration, never before.
            let mut taken_at = None;
            for second in 1..=8u64 {
                let now = start + Duration::from_secs(second);
                a.look(now, 0).await;
                if let Stand::Shard { mine, .. } = c.look(now, 0).await {
                    assert_eq!(mine, 1);
                    taken_at = Some(second);
                    break;
                }
            }
            let taken_at = taken_at.expect("the standby took the lapsed shard");
            assert!((6..=7).contains(&taken_at), "taken after {taken_at} s");
            assert_eq!(api.holder("feed-1-shard-1"), "c");
            assert_eq!(
                api.holder("feed-1-shard-0"),
                "a",
                "the live shard is never taken"
            );
            // The pod that was shard 1, back from its pause, finds it gone.
            let later = start + Duration::from_secs(9);
            assert!(
                matches!(b.look(later, 0).await, Stand::Lost(why) if why.contains("held by c"))
            );
            assert_eq!(a.look(later, 0).await, shard(0, &[1]));
        });
    }

    #[test]
    fn a_live_shard_reads_a_lapsed_shard_as_gone_and_as_live_again_when_it_is_retaken() {
        run(async {
            let api = Arc::new(Memory::default());
            let (mut a, mut b) = (pod(&api, 2, "a"), pod(&api, 2, "b"));
            let start = Instant::now();
            a.look(start, 0).await;
            b.look(start, 0).await;
            assert_eq!(a.look(start + RENEW, 0).await, shard(0, &[1]));
            // `b` stops; with no standby its Lease lapses and stays lapsed.
            let mut gone_at = None;
            for second in 3..=10u64 {
                if a.look(start + Duration::from_secs(second), 0).await == shard(0, &[]) {
                    gone_at = Some(second);
                    break;
                }
            }
            let gone_at = gone_at.expect("the lapsed shard read as gone");
            assert!((8..=9).contains(&gone_at), "read as gone after {gone_at} s");
            // Its pod restarted: a new process takes shard 1 again.
            let mut again = pod(&api, 2, "b2");
            let now = start + Duration::from_secs(12);
            // It sees shard 0 held, then shard 1 lapsed once it has watched
            // it for the lease's duration.
            assert_eq!(again.look(now, 0).await, Stand::Standby);
            a.look(now, 0).await;
            let now = now + LASTS;
            a.look(now, 0).await;
            assert_eq!(again.look(now, 0).await, shard(1, &[0]));
            assert_eq!(a.look(now + RENEW, 0).await, shard(0, &[1]));
        });
    }

    #[test]
    fn a_shard_released_is_the_standby_s_at_its_next_look() {
        run(async {
            let api = Arc::new(Memory::default());
            let (mut a, mut c) = (pod(&api, 1, "a"), pod(&api, 1, "c"));
            let start = Instant::now();
            assert_eq!(a.look(start, 0).await, shard(0, &[]));
            assert_eq!(c.look(start, 0).await, Stand::Standby);
            a.release(0).await;
            assert_eq!(
                c.look(start + Duration::from_secs(1), 0).await,
                shard(0, &[])
            );
            // The pod that released it never acts as the shard again.
            assert!(matches!(
                a.look(start + Duration::from_secs(2), 0).await,
                Stand::Lost(_)
            ));
        });
    }

    /// No epoch fences a plugin: a shard cut off from the API server stops
    /// before a standby can take its Lease, never after.
    #[test]
    fn a_shard_that_cannot_renew_stops_before_its_lease_can_lapse() {
        run(async {
            let api = Arc::new(Memory::default());
            let (mut a, mut c) = (pod(&api, 1, "a"), pod(&api, 1, "c"));
            let start = Instant::now();
            assert_eq!(a.look(start, 0).await, shard(0, &[]));
            assert_eq!(c.look(start, 0).await, Stand::Standby);
            api.down.store(true, Ordering::SeqCst);
            // One renewal missed: still the shard.
            assert_eq!(a.look(start + RENEW, 0).await, shard(0, &[]));
            // The lease's duration less one renewal unrenewed: it stops.
            let stopped = a.look(start + Duration::from_secs(4), 0).await;
            assert!(
                matches!(&stopped, Stand::Lost(why) if why.contains("could not be renewed")),
                "{stopped:?}"
            );
            // The API server answers again; the standby, which saw the
            // Lease last at the start, takes it no sooner than its lapse.
            api.down.store(false, Ordering::SeqCst);
            assert_eq!(
                c.look(start + Duration::from_secs(5), 0).await,
                Stand::Standby
            );
            assert_eq!(
                c.look(start + Duration::from_secs(11), 0).await,
                shard(0, &[])
            );
        });
    }

    // ── In time: the loops a sidecar runs, on a paused clock ────────────────

    use tokio::sync::watch;
    use tokio::time::Instant as At;

    /// One pod's way to the API server: slow, failing or cut, as told.
    #[derive(Default)]
    struct Link {
        store: Arc<Memory>,
        /// Requests naming a Lease whose name contains the text wait this
        /// long first ("" is every Lease).
        slow: Mutex<Vec<(String, Duration)>>,
        /// A slow request fails once it has waited, as one timed out does.
        slow_fails: AtomicBool,
        /// The next request alone waits this long, and is then answered.
        slow_once: Mutex<Option<Duration>>,
        /// The next request alone fails.
        fail_once: AtomicBool,
    }

    impl Link {
        fn to(store: &Arc<Memory>) -> Arc<Self> {
            Arc::new(Self {
                store: Arc::clone(store),
                ..Self::default()
            })
        }
        fn slow(&self, name: &str, wait: Duration, fails: bool) {
            self.slow.lock().unwrap().push((name.to_string(), wait));
            self.slow_fails.store(fails, Ordering::SeqCst);
        }
        async fn pass(&self, name: &str) -> Result<(), String> {
            let once = self.slow_once.lock().unwrap().take();
            if let Some(wait) = once {
                tokio::time::sleep(wait).await;
            }
            let wait = self
                .slow
                .lock()
                .unwrap()
                .iter()
                .find(|(part, _)| name.contains(part.as_str()))
                .map(|(_, wait)| *wait);
            if let Some(wait) = wait {
                tokio::time::sleep(wait).await;
                if self.slow_fails.load(Ordering::SeqCst) {
                    return Err("the API server did not answer in time".into());
                }
            }
            if self.fail_once.swap(false, Ordering::SeqCst) {
                return Err("the API server answered 500".into());
            }
            Ok(())
        }
    }

    impl Leases for Link {
        async fn get(&self, name: &str) -> Result<Option<Record>, String> {
            self.pass(name).await?;
            self.store.get(name).await
        }
        async fn create(&self, name: &str, record: &Record) -> Result<Wrote, String> {
            self.pass(name).await?;
            self.store.create(name, record).await
        }
        async fn update(&self, name: &str, record: &Record) -> Result<Wrote, String> {
            self.pass(name).await?;
            self.store.update(name, record).await
        }
    }

    fn on(link: &Arc<Link>, count: u32, holder: &str) -> ShardSet<Link> {
        link.store.make_shards(count);
        ShardSet::new(Arc::clone(link), "feed-1", count, holder, RENEW, LASTS).unwrap()
    }

    /// A pod that takes a shard and keeps it: when its keeping ended, and how.
    fn serving(
        set: ShardSet<Link>,
        stop: &watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<(At, Kept)> {
        let stop = stop.clone();
        tokio::spawn(async move {
            let (set, _, _) = set.take(stop.clone()).await.unwrap().expect("a shard");
            let kept = set.keep(|_| {}, stop).await;
            (At::now(), kept)
        })
    }

    /// A pod standing by: when a shard was its own, and which.
    fn standing_by(
        set: ShardSet<Link>,
        stop: &watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<Option<(At, u32, Vec<u32>)>> {
        let stop = stop.clone();
        tokio::spawn(async move {
            let taken = set.take(stop).await.unwrap();
            taken.map(|(_, mine, others)| (At::now(), mine, others))
        })
    }

    fn secs(seconds: f64) -> Duration {
        Duration::from_secs_f64(seconds)
    }

    /// Review finding 1: the holder's pod is cut off from the API server
    /// alone, each request timing out. No epoch fences a plugin, so it has
    /// stopped before the standby acts as its shard.
    #[tokio::test(start_paused = true)]
    async fn a_shard_whose_requests_time_out_has_stopped_before_the_standby_takes_its_lease() {
        let store = Arc::new(Memory::default());
        let (to_a, to_c) = (Link::to(&store), Link::to(&store));
        let (_stopper, stop) = watch::channel(false);
        let a = serving(on(&to_a, 1, "a"), &stop);
        tokio::time::sleep(secs(0.1)).await;
        let c = standing_by(on(&to_c, 1, "c"), &stop);
        tokio::time::sleep(secs(5.0)).await;
        to_a.slow("", RENEW, true);
        let (lost_at, kept) = a.await.unwrap();
        assert!(
            matches!(&kept, Kept::Lost(why) if why.contains("could not be renewed")),
            "{kept:?}"
        );
        let (held_at, mine, _) = c.await.unwrap().expect("the standby took the shard");
        assert_eq!(mine, 0);
        assert!(
            lost_at < held_at,
            "the holder stopped {:?} after the standby held its lease",
            lost_at.duration_since(held_at)
        );
    }

    /// Review finding 1, the standby's side: a Lease read slowly is held
    /// lapsed from when the read answered, never from when it was asked --
    /// the write it shows may have landed anywhere between.
    #[tokio::test(start_paused = true)]
    async fn a_standby_counts_a_lease_s_lapse_from_when_its_read_answered() {
        let store = Arc::new(Memory::default());
        let (to_a, to_c) = (Link::to(&store), Link::to(&store));
        let (_stopper, stop) = watch::channel(false);
        let a = serving(on(&to_a, 1, "a"), &stop);
        tokio::time::sleep(secs(0.1)).await;
        let c = standing_by(on(&to_c, 1, "c"), &stop);
        // The standby's read asked at 7 s answers at 8.5 s, with the
        // holder's renewal of 8 s in it; then the holder's pod is gone.
        tokio::time::sleep(secs(6.4)).await;
        *to_c.slow_once.lock().unwrap() = Some(secs(1.5));
        tokio::time::sleep(secs(2.0)).await;
        a.abort();
        let written_at = store.written_at("feed-1-shard-0");
        let (held_at, _, _) = c.await.unwrap().expect("the standby took the shard");
        assert!(
            held_at.duration_since(written_at) >= LASTS,
            "taken {:?} after its holder's last renewal; a lease lasts {LASTS:?}",
            held_at.duration_since(written_at)
        );
    }

    /// Review finding 2: reading the other shards' Leases, however slowly,
    /// never delays a shard's own renewal.
    #[tokio::test(start_paused = true)]
    async fn a_shard_reading_its_neighbours_slowly_still_renews_and_keeps_its_lease() {
        let store = Arc::new(Memory::default());
        let links: Vec<_> = (0..4).map(|_| Link::to(&store)).collect();
        let (_stopper, stop) = watch::channel(false);
        let a = serving(on(&links[0], 3, "a"), &stop);
        tokio::time::sleep(secs(0.1)).await;
        let b = serving(on(&links[1], 3, "b"), &stop);
        tokio::time::sleep(secs(0.1)).await;
        let d = serving(on(&links[2], 3, "d"), &stop);
        tokio::time::sleep(secs(0.1)).await;
        let c = standing_by(on(&links[3], 3, "c"), &stop);
        // Shard 0's reads of the other two each time out.
        links[0].slow("shard-1", secs(2.5), true);
        links[0].slow("shard-2", secs(2.5), true);
        tokio::time::sleep(secs(40.0)).await;
        assert!(
            !c.is_finished(),
            "the standby took a live shard: {:?}",
            c.await
        );
        for (shard, holder) in [(0, "a"), (1, "b"), (2, "d")] {
            assert_eq!(store.holder(&lease_name("feed-1", shard)), holder);
        }
        assert!(!a.is_finished() && !b.is_finished() && !d.is_finished());
    }

    /// Review finding 4: a pod that takes a shard reads the others in that
    /// same look, so it never stands as the only live shard for a renewal
    /// and takes every subject.
    #[tokio::test(start_paused = true)]
    async fn a_standby_that_takes_a_shard_first_stands_with_the_others_live() {
        let store = Arc::new(Memory::default());
        let links: Vec<_> = (0..3).map(|_| Link::to(&store)).collect();
        let (_stopper, stop) = watch::channel(false);
        let a = serving(on(&links[0], 2, "a"), &stop);
        tokio::time::sleep(secs(0.1)).await;
        let b = serving(on(&links[1], 2, "b"), &stop);
        tokio::time::sleep(secs(0.1)).await;
        let c = standing_by(on(&links[2], 2, "c"), &stop);
        tokio::time::sleep(secs(4.5)).await;
        b.abort();
        let (_, mine, others) = c.await.unwrap().expect("the standby took the shard");
        assert_eq!((mine, others), (1, vec![0]));
        assert!(!a.is_finished());
    }

    /// Review finding 7: one read of the Lease failing while the standby
    /// waits out its lapse costs no second lapse: the shard is its own
    /// within 10 s of its holder's end.
    #[tokio::test(start_paused = true)]
    async fn a_standby_whose_read_failed_once_still_takes_the_shard_within_ten_seconds() {
        let store = Arc::new(Memory::default());
        let (to_a, to_c) = (Link::to(&store), Link::to(&store));
        let (_stopper, stop) = watch::channel(false);
        let a = serving(on(&to_a, 1, "a"), &stop);
        tokio::time::sleep(secs(0.1)).await;
        let c = standing_by(on(&to_c, 1, "c"), &stop);
        tokio::time::sleep(secs(8.0)).await;
        a.abort();
        let (ended, written_at) = (At::now(), store.written_at("feed-1-shard-0"));
        tokio::time::sleep(secs(4.4)).await;
        to_c.fail_once.store(true, Ordering::SeqCst);
        let (held_at, _, _) = c.await.unwrap().expect("the standby took the shard");
        assert!(
            held_at.duration_since(ended) <= secs(10.0),
            "taken {:?} after its holder ended",
            held_at.duration_since(ended)
        );
        // And never before the lease's own lapse.
        assert!(held_at.duration_since(written_at) >= LASTS);
    }

    /// Review finding 6: a pod told to stop while it waits for a shard, a
    /// look in flight, stops; it holds no Lease after, so no shard is left
    /// to lapse under a pod that is gone.
    #[tokio::test(start_paused = true)]
    async fn a_pod_told_to_stop_while_its_look_is_in_flight_stops_holding_no_lease() {
        let store = Arc::new(Memory::default());
        let to_a = Link::to(&store);
        to_a.slow("", secs(1.5), false);
        let (stopper, stop) = watch::channel(false);
        let a = standing_by(on(&to_a, 1, "a"), &stop);
        tokio::time::sleep(secs(0.5)).await;
        stopper.send(true).unwrap();
        assert_eq!(a.await.unwrap(), None);
        assert_eq!(store.holder("feed-1-shard-0"), "");

        // And one standing by, every Lease held, stops at once.
        let to_b = Link::to(&store);
        let (stopper, stop) = watch::channel(false);
        let _b = serving(on(&to_b, 1, "b"), &stop);
        tokio::time::sleep(secs(0.1)).await;
        let (c_stopper, c_stop) = watch::channel(false);
        let c = standing_by(on(&Link::to(&store), 1, "c"), &c_stop);
        tokio::time::sleep(secs(3.0)).await;
        let told = At::now();
        c_stopper.send(true).unwrap();
        assert_eq!(c.await.unwrap(), None);
        assert!(At::now().duration_since(told) < secs(0.1));
        drop(stopper);
    }

    // ── The epoch fence (step 3d, checkpoint 5, item 32) ────────────────────

    /// A pod that writes on, fenced: takes a shard, has its epoch raised
    /// in `fence` as the lake raises it, and keeps it; when its keeping
    /// ended, how, and the epoch it writes under.
    fn serving_fenced(
        set: ShardSet<Link>,
        stop: &watch::Receiver<bool>,
        fence: &Arc<meridian_changes::shard_fence::Memory>,
        epoch: &Arc<Mutex<u64>>,
        renewal: &Arc<Mutex<Option<Renewal>>>,
    ) -> tokio::task::JoinHandle<(At, Kept)> {
        let (stop, fence, epoch) = (stop.clone(), Arc::clone(fence), Arc::clone(epoch));
        let renewal = Arc::clone(renewal);
        tokio::spawn(async move {
            let (set, mine, _) = set
                .writing_on()
                .take(stop.clone())
                .await
                .unwrap()
                .expect("a shard");
            *renewal.lock().unwrap() = Some(set.renewal());
            let taken = set.taken().await.unwrap();
            let taking = meridian_changes::shard_fence::Taking {
                lease_transitions: taken.transitions,
                lease_uid: taken.lease_uid,
                admit: None,
            };
            *epoch.lock().unwrap() = fence.take("feed-1", mine, &taking).unwrap();
            let kept = set.keep(|_| {}, stop).await;
            (At::now(), kept)
        })
    }

    /// Item 32's paused-clock test: a data shard whose API calls hang writes
    /// on past its renew deadline -- fenced, not stopped by the clock; the
    /// standby takes the Lease once it lapses and the shard's epoch is
    /// raised by one; the holder's next write, under its old epoch, is
    /// refused.
    #[tokio::test(start_paused = true)]
    async fn a_data_shard_whose_api_calls_hang_serves_on_and_is_fenced_once_the_standby_takes() {
        let store = Arc::new(Memory::default());
        let (to_a, to_c) = (Link::to(&store), Link::to(&store));
        let fence = Arc::new(meridian_changes::shard_fence::Memory::default());
        let (a_epoch, c_epoch) = (Arc::new(Mutex::new(0)), Arc::new(Mutex::new(0)));
        let (a_renewal, c_renewal) = (Arc::new(Mutex::new(None)), Arc::new(Mutex::new(None)));
        let (_stopper, stop) = watch::channel(false);
        let a = serving_fenced(on(&to_a, 1, "a"), &stop, &fence, &a_epoch, &a_renewal);
        tokio::time::sleep(secs(0.1)).await;
        let c = serving_fenced(on(&to_c, 1, "c"), &stop, &fence, &c_epoch, &c_renewal);
        tokio::time::sleep(secs(5.0)).await;
        let cut_at = At::now();
        let renewed = store.written_at("feed-1-shard-0");
        // The holder alone cut from the API server: its every call hangs.
        to_a.slow("", secs(3_600.0), true);
        tokio::time::sleep(secs(20.0)).await;
        // Past its renew deadline it serves on: nothing but another holder
        // or a fenced write ends it.
        assert!(
            !a.is_finished(),
            "the holder stopped by the clock: {:?}",
            a.await
        );
        assert!(At::now().duration_since(cut_at) > secs(4.0));
        // The standby took the lapsed Lease, never before it lapsed, and
        // the shard's epoch is the holder's plus one.
        assert_eq!(store.holder("feed-1-shard-0"), "c");
        assert!(store.written_at("feed-1-shard-0").duration_since(renewed) >= LASTS);
        let (old, new) = (*a_epoch.lock().unwrap(), *c_epoch.lock().unwrap());
        assert_eq!((old, new), (1, 2));
        // Past its renew deadline it sends its lake record writes alone
        // (review c5a M1): a want's renewal, a ticket, its liveness wait.
        let gate = send_gate(a_renewal.lock().unwrap().clone().expect("a's renewal"));
        assert!(gate("platform.lake.command.record-prices").is_ok());
        assert!(gate("platform.lake.internal.write-ahead.feed-1.prices").is_ok());
        assert!(gate("platform.lake.query.read-observations").is_ok());
        for held_back in [
            "platform.lake.command.renew-wants",
            "platform.lake.command.decline-want",
            "platform.lake.command.record-liveness",
            "platform.config.command.file-ticket",
            "platform.deployment.command.report-latency",
        ] {
            assert!(gate(held_back).is_err(), "{held_back} was sent unrenewed");
        }
        // The new holder, renewing, sends everything.
        let c_gate = send_gate(c_renewal.lock().unwrap().clone().expect("c's renewal"));
        assert!(c_gate("platform.lake.command.renew-wants").is_ok());
        // The holder's next write is refused; the new holder's passes.
        let stamp = |epoch| meridian_changes::shard_fence::ShardStamp::of("feed-1", 0, epoch);
        let refused = fence.check(&[stamp(old).as_ref()]);
        assert_eq!(refused[0].as_ref().unwrap_err().held, new);
        assert_eq!(fence.check(&[stamp(new).as_ref()]), vec![Ok(())]);
        // The API server answering again, the holder finds another holder.
        to_a.slow.lock().unwrap().clear();
        let (_, kept) = a.await.unwrap();
        assert!(
            matches!(&kept, Kept::Lost(why) if why.contains("held by c")),
            "{kept:?}"
        );
        drop(c);
    }

    /// Review c5a5 F5: a data shard's renewal is given its one renewal from
    /// when its tick fires, never from before the tick: with the API server
    /// answering at once, no renewal is given up and none is late.
    #[tokio::test(start_paused = true)]
    async fn a_data_shard_s_renewals_each_get_a_whole_renewal_and_none_is_given_up() {
        let store = Arc::new(Memory::default());
        let to_a = Link::to(&store);
        to_a.slow("", secs(0.01), false);
        let (_stopper, stop) = watch::channel(false);
        let a = tokio::spawn(async move {
            let (set, _, _) = on(&to_a, 1, "a")
                .writing_on()
                .take(stop.clone())
                .await
                .unwrap()
                .expect("a shard");
            set.keep(|_| {}, stop).await
        });
        tokio::time::sleep(secs(0.1)).await;
        let (mut last, mut longest) = (store.written_at("feed-1-shard-0"), Duration::ZERO);
        for _ in 0..400 {
            tokio::time::sleep(secs(0.05)).await;
            let written = store.written_at("feed-1-shard-0");
            if written != last {
                longest = longest.max(written.duration_since(last));
                last = written;
            }
        }
        assert!(!a.is_finished());
        assert!(
            longest <= RENEW + secs(0.1),
            "renewals {longest:?} apart; each is due every {RENEW:?}"
        );
    }

    /// Review c5a5 F4: a data shard's standby never counts an API server
    /// outage it did not observe toward its holder's lapse (item 32's Rule:
    /// shards and standbys write on through it), as a store's member does
    /// not (CI 42958621081): where the API answers the standby first, it
    /// waits a whole lapse from then, and the holder keeps its shard.
    #[test]
    fn a_data_shard_s_standby_never_takes_the_shard_for_an_outage_it_did_not_see() {
        run(async {
            let api = Arc::new(Memory::default());
            let (mut a, mut c) = (
                pod(&api, 1, "a").writing_on(),
                pod(&api, 1, "c").writing_on(),
            );
            let start = Instant::now();
            assert_eq!(a.look(start, 0).await, shard(0, &[]));
            assert_eq!(c.look(start, 0).await, Stand::Standby);
            // 15 s with no API server, for both pods: the holder writes on.
            api.down.store(true, Ordering::SeqCst);
            for second in (2..=14).step_by(2) {
                let now = start + Duration::from_secs(second);
                assert_eq!(a.look(now, 0).await, shard(0, &[]));
                assert_eq!(c.look(now, 0).await, Stand::Standby);
            }
            // The API answers again: the standby looks before the holder
            // renews, and stands by.
            api.down.store(false, Ordering::SeqCst);
            let back = start + Duration::from_secs(15);
            assert_eq!(c.look(back, 0).await, Stand::Standby);
            assert_eq!(a.look(back, 0).await, shard(0, &[]));
            assert_eq!(
                c.look(back + RENEW, 0).await,
                Stand::Standby,
                "the standby took a shard its holder renews"
            );
            assert_eq!(api.holder("feed-1-shard-0"), "a");
        });
    }

    /// Review c5a6 M1: a data shard's own Lease is kept as it was last seen
    /// through a failed look -- only its standby's lapse count drops what
    /// it saw (F4) -- so one API error after a take neither fails the take
    /// nor leaves the Lease held by a name that is gone: the take still
    /// knows its transitions, and a release still writes.
    #[test]
    fn a_data_shard_s_own_lease_is_kept_as_seen_through_a_failed_look_and_released_after() {
        run(async {
            let api = Arc::new(Memory::default());
            let mut a = pod(&api, 1, "a").writing_on();
            let start = Instant::now();
            assert_eq!(a.look(start, 0).await, shard(0, &[]));
            api.down.store(true, Ordering::SeqCst);
            assert_eq!(
                a.look(start + Duration::from_secs(1), 0).await,
                shard(0, &[])
            );
            api.down.store(false, Ordering::SeqCst);
            assert!(
                a.taken().await.is_some(),
                "one failed look lost the take's record"
            );
            a.release(0).await;
            assert_eq!(
                api.holder("feed-1-shard-0"),
                "",
                "the release wrote nothing"
            );
        });
    }

    // ── A data standby's takeover within 10 s (PO item 54, 2026-11-16) ──

    /// The fence raise at its usual cost, and more: CI 43973174092's taker
    /// went from its take to serving in 21 ms.
    const RAISE: Duration = Duration::from_millis(500);
    /// Each of the standby's reads of the API server, and how much slower
    /// the one that first sees the renewal answers.
    const READ: Duration = Duration::from_millis(50);
    /// Reads near the shard client's 1 s timeout (review c5a10 L2).
    const SLOW_READ: Duration = Duration::from_millis(950);

    /// A data standby: when it took a shard's Lease, and when it served,
    /// its epoch raised in `fence` at [`RAISE`]'s cost.
    fn standing_by_fenced(
        set: ShardSet<Link>,
        stop: &watch::Receiver<bool>,
        fence: &Arc<meridian_changes::shard_fence::Memory>,
    ) -> tokio::task::JoinHandle<Option<(At, At)>> {
        let (stop, fence) = (stop.clone(), Arc::clone(fence));
        tokio::spawn(async move {
            let (set, mine, _) = set.writing_on().take(stop).await.unwrap()?;
            let took = At::now();
            tokio::time::sleep(RAISE).await;
            let taken = set.taken().await.expect("the take's record");
            let taking = meridian_changes::shard_fence::Taking {
                lease_transitions: taken.transitions,
                lease_uid: taken.lease_uid,
                admit: None,
            };
            fence.take("feed-1", mine, &taking).unwrap();
            Some((took, At::now()))
        })
    }

    /// Spec section 7's takeover (≤ 10 s) for a data shard, at its worst
    /// with a standby that can read the API server: the holder renews just
    /// before it ends; the standby's look before saw the Lease a moment
    /// before that renewal, so it first sees it a look later, and that
    /// read answers slowly while the reads after answer at once, so the
    /// lapse falls just short at one look and the take waits for the next.
    /// It takes no sooner than the lapse from that renewal, and serves,
    /// its epoch raised, within 10 s of the end. `end` ends the holder.
    async fn a_data_standby_at_its_worst_phase_serves_within_ten_seconds(
        read: Duration,
        slower: Duration,
        end: impl FnOnce(&Arc<Link>, tokio::task::JoinHandle<(At, Kept)>),
    ) -> (Duration, Duration) {
        let store = Arc::new(Memory::default());
        let (to_a, to_c) = (Link::to(&store), Link::to(&store));
        let fence = Arc::new(meridian_changes::shard_fence::Memory::default());
        let (epoch, renewal) = (Arc::new(Mutex::new(0)), Arc::new(Mutex::new(None)));
        let (_stopper, stop) = watch::channel(false);
        let t0 = At::now();
        let a = serving_fenced(on(&to_a, 1, "a"), &stop, &fence, &epoch, &renewal);
        // a renews at t0 + 2 s, 4 s, ...; c looks at t0 + 0.999 s - read
        // + n s, each read answering `read` later: 1 ms before each second.
        tokio::time::sleep_until(t0 + secs(0.999) - read).await;
        to_c.slow("", read, false);
        let c = standing_by_fenced(on(&to_c, 1, "c"), &stop, &fence);
        // The holder renewed at t0 + 2 s, 1 ms after c's read; ended now,
        // it renews no more.
        tokio::time::sleep_until(t0 + secs(2.001)).await;
        let renewed = store.written_at("feed-1-shard-0");
        assert_eq!(renewed, t0 + RENEW, "the holder's renewal");
        let ended = At::now();
        end(&to_a, a);
        // c's look at t0 + 2.999 s - read, the first to see that renewal,
        // answers `slower` later than the others.
        *to_c.slow_once.lock().unwrap() = Some(slower);
        let (took, served) = c.await.unwrap().expect("the standby took the shard");
        assert_eq!(store.holder("feed-1-shard-0"), "c");
        assert!(
            took.duration_since(renewed) >= LASTS,
            "taken {:?} after the holder's last renewal; a lease lasts {LASTS:?}",
            took.duration_since(renewed)
        );
        assert!(
            served.duration_since(ended) <= secs(10.0),
            "served {:?} after the holder ended",
            served.duration_since(ended)
        );
        (took.duration_since(ended), served.duration_since(ended))
    }

    /// PO item 54 (Kubernetes' observedTime): a data standby holds a Lease
    /// seen when its read answered, never when it was asked -- the renewal
    /// it shows may have landed anywhere between -- so it takes no sooner
    /// than the lapse from that renewal.
    #[tokio::test(start_paused = true)]
    async fn a_data_standby_counts_a_lease_s_lapse_from_when_its_read_answered() {
        let store = Arc::new(Memory::default());
        let (to_a, to_c) = (Link::to(&store), Link::to(&store));
        let fence = Arc::new(meridian_changes::shard_fence::Memory::default());
        let (epoch, renewal) = (Arc::new(Mutex::new(0)), Arc::new(Mutex::new(None)));
        let (_stopper, stop) = watch::channel(false);
        let a = serving_fenced(on(&to_a, 1, "a"), &stop, &fence, &epoch, &renewal);
        tokio::time::sleep(secs(0.1)).await;
        let c = standing_by_fenced(on(&to_c, 1, "c"), &stop, &fence);
        // The standby's read asked at 7.1 s answers at 8.6 s, with the
        // holder's renewal of 8 s in it; then the holder's pod is gone.
        tokio::time::sleep(secs(6.4)).await;
        *to_c.slow_once.lock().unwrap() = Some(secs(1.5));
        tokio::time::sleep(secs(2.0)).await;
        a.abort();
        let written_at = store.written_at("feed-1-shard-0");
        let (took, _) = c.await.unwrap().expect("the standby took the shard");
        assert!(
            took.duration_since(written_at) >= LASTS,
            "taken {:?} after its holder's last renewal; a lease lasts {LASTS:?}",
            took.duration_since(written_at)
        );
    }

    /// PO item 54, the cut: the holder alone cut from the API server just
    /// after its renewal -- its every call hanging, writing on -- and its
    /// standby, which can read, serves the shard within 10 s of the cut.
    #[tokio::test(start_paused = true)]
    async fn a_data_holder_cut_just_after_its_renewal_is_served_by_its_standby_within_ten_seconds()
    {
        let cut = |to_a: &Arc<Link>, a: tokio::task::JoinHandle<(At, Kept)>| {
            to_a.slow("", secs(3_600.0), true);
            drop(a);
        };
        let (took, served) =
            a_data_standby_at_its_worst_phase_serves_within_ten_seconds(READ, READ, cut).await;
        // One look late to see the renewal, a lapse, one look of the
        // grid's phase, a slow read, the take's write, the raise.
        assert_eq!((took, served), (secs(8.048), secs(8.548)));
        // Reads near their 1 s timeout: 0.9 s more, still within 10 s with
        // the raise at 500 ms (review c5a10 L2).
        let (took, served) =
            a_data_standby_at_its_worst_phase_serves_within_ten_seconds(SLOW_READ, secs(0.04), cut)
                .await;
        assert_eq!((took, served), (secs(8.948), secs(9.448)));
    }

    /// PO item 54, the kill: the holder's pod gone just after its renewal,
    /// and its standby serves the shard within 10 s of the kill.
    #[tokio::test(start_paused = true)]
    async fn a_data_holder_killed_just_after_its_renewal_is_served_by_its_standby_within_ten_seconds(
    ) {
        let kill = |_: &Arc<Link>, a: tokio::task::JoinHandle<(At, Kept)>| a.abort();
        let (took, served) =
            a_data_standby_at_its_worst_phase_serves_within_ten_seconds(READ, READ, kill).await;
        assert_eq!((took, served), (secs(8.048), secs(8.548)));
        let (took, served) = a_data_standby_at_its_worst_phase_serves_within_ten_seconds(
            SLOW_READ,
            secs(0.04),
            kill,
        )
        .await;
        assert_eq!((took, served), (secs(8.948), secs(9.448)));
    }

    /// Item 49 (ruled 2026-11-16; review c5a5 F3): a pod that loses its
    /// shard stands by again in its own process, under a new holder name,
    /// and takes the shard at its next look once it is free -- no process
    /// ended, so no back-off waited out.
    #[tokio::test(start_paused = true)]
    async fn a_shard_lost_stands_by_again_in_its_process_and_takes_the_shard_once_it_is_free() {
        let store = Arc::new(Memory::default());
        let (to_a, to_b) = (Link::to(&store), Link::to(&store));
        let (a_stopper, a_stop) = watch::channel(false);
        let rounds = Arc::new(Mutex::new(Vec::<(String, Served)>::new()));
        let a = tokio::spawn({
            let (to_a, rounds, stop) = (Arc::clone(&to_a), Arc::clone(&rounds), a_stop.clone());
            let mut made = 0;
            serve_shards(
                move || {
                    made += 1;
                    Ok(on(&to_a, 1, &format!("a{made}")).writing_on())
                },
                move |set, _, _| {
                    let (rounds, stop) = (Arc::clone(&rounds), stop.clone());
                    async move {
                        let holder = set.taken().await.expect("taken").holder;
                        let served = match set.keep(|_| {}, stop).await {
                            Kept::Released => Served::Stopped,
                            Kept::Lost(why) => Served::Lost(why),
                        };
                        rounds.lock().unwrap().push((holder, served.clone()));
                        Ok(served)
                    }
                },
                a_stop,
            )
        });
        tokio::time::sleep(secs(0.1)).await;
        let (b_stopper, b_stop) = watch::channel(false);
        let b = serving(on(&to_b, 1, "b"), &b_stop);
        tokio::time::sleep(secs(1.0)).await;
        assert_eq!(store.holder("feed-1-shard-0"), "a1");
        // `a` alone cut from the API server: it writes on; `b` takes the
        // lapsed Lease. The API answering `a` again, it finds `b`.
        to_a.slow("", secs(3_600.0), true);
        tokio::time::sleep(secs(10.0)).await;
        assert_eq!(store.holder("feed-1-shard-0"), "b");
        to_a.slow.lock().unwrap().clear();
        tokio::time::sleep(secs(6.0)).await;
        {
            let rounds = rounds.lock().unwrap();
            assert_eq!(rounds.len(), 1, "{rounds:?}");
            assert_eq!(rounds[0].0, "a1");
            assert!(
                matches!(&rounds[0].1, Served::Lost(why) if why.contains("held by b")),
                "{rounds:?}"
            );
        }
        assert!(!a.is_finished(), "the pod's serving ended: {:?}", a.await);
        // `b` stops and gives the shard up: `a`, standing by under a new
        // name, takes it at its next look.
        b_stopper.send(true).unwrap();
        assert_eq!(b.await.unwrap().1, Kept::Released);
        tokio::time::sleep(secs(1.5)).await;
        assert_eq!(store.holder("feed-1-shard-0"), "a2");
        a_stopper.send(true).unwrap();
        assert_eq!(a.await.unwrap(), Ok(()));
        assert_eq!(
            rounds.lock().unwrap().last(),
            Some(&("a2".to_string(), Served::Stopped))
        );
        assert_eq!(store.holder("feed-1-shard-0"), "");
    }

    /// Item 49: a take that did not complete gives its Lease up and stands
    /// by again after a lapse, in its process.
    #[tokio::test(start_paused = true)]
    async fn a_take_not_completed_stands_by_again_after_a_lapse() {
        let store = Arc::new(Memory::default());
        let to_a = Link::to(&store);
        let (a_stopper, a_stop) = watch::channel(false);
        let taken = Arc::new(Mutex::new(Vec::<(String, At)>::new()));
        let a = tokio::spawn({
            let (taken, stop) = (Arc::clone(&taken), a_stop.clone());
            let mut made = 0;
            serve_shards(
                move || {
                    made += 1;
                    Ok(on(&to_a, 1, &format!("a{made}")).writing_on())
                },
                move |mut set, _, _| {
                    let (taken, stop) = (Arc::clone(&taken), stop.clone());
                    async move {
                        let holder = set.taken().await.expect("taken").holder;
                        let first = taken.lock().unwrap().is_empty();
                        taken.lock().unwrap().push((holder, At::now()));
                        if first {
                            set.release(0).await;
                            return Ok(Served::NotTaken("the epoch was not raised".into()));
                        }
                        Ok(match set.keep(|_| {}, stop).await {
                            Kept::Released => Served::Stopped,
                            Kept::Lost(why) => Served::Lost(why),
                        })
                    }
                },
                a_stop,
            )
        });
        tokio::time::sleep(secs(10.0)).await;
        let taken = taken.lock().unwrap().clone();
        assert_eq!(taken.len(), 2, "{taken:?}");
        assert_eq!((taken[0].0.as_str(), taken[1].0.as_str()), ("a1", "a2"));
        assert!(taken[1].1.duration_since(taken[0].1) >= LASTS);
        assert!(!a.is_finished());
        a_stopper.send(true).unwrap();
        assert_eq!(a.await.unwrap(), Ok(()));
    }

    /// Another role's shard keeps the 4 s stop (item 32: a property of the
    /// role): the fence cannot stop an order path's venue send.
    #[test]
    fn only_a_data_plugin_writes_on() {
        let roles = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        assert!(writes_on(&roles(&["data"])));
        for other in [
            "oms",
            "ems",
            "connectivity",
            "custody",
            "reporting",
            "signal",
        ] {
            assert!(!writes_on(&roles(&[other])), "{other}");
        }
        assert!(!writes_on(&roles(&["data", "connectivity"])));
        assert!(!writes_on(&[]));
    }

    /// Item 32's sidecar test: a pod that took a shard has the lake raise
    /// its epoch, naming the Lease's transitions and whom it took it from,
    /// and its bus stamps the epoch answered on every message after.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_shard_taken_has_its_epoch_raised_and_its_bus_stamps_it_on_every_message() {
        use meridian_bus::{Bus, MemoryBackend};
        use prost::Message as _;
        let api = Arc::new(Memory::default());
        let (mut a, mut b) = (pod(&api, 1, "a"), pod(&api, 1, "b"));
        let start = Instant::now();
        assert_eq!(a.look(start, 0).await, shard(0, &[]));
        assert_eq!(b.look(start, 0).await, Stand::Standby);
        // `a` stops renewing; `b` takes the lapsed Lease.
        assert_eq!(
            b.look(start + Duration::from_secs(7), 0).await,
            shard(0, &[])
        );
        let bus = Bus::single(
            "feed-1",
            Arc::new(MemoryBackend::new()),
            Arc::new(meridian_clock::ManualClock::at(1)),
        );
        let asked = Arc::new(Mutex::new(Vec::new()));
        let asking = Arc::clone(&asked);
        bus.serve(meridian_lake::service::TAKE_SHARD, move |envelope| {
            let request =
                meridian_domain::v1::TakeShardRequest::decode(&envelope.payload[..]).unwrap();
            let meta = envelope.meta.clone().unwrap_or_default();
            asking.lock().unwrap().push((request, meta.shard_epoch));
            Ok((
                "meridian.v1.TakeShardReply".into(),
                meridian_domain::v1::TakeShardReply { epoch: 7 }.encode_to_vec(),
            ))
        });
        let seen = Arc::new(Mutex::new(None));
        let seeing = Arc::clone(&seen);
        bus.serve("platform.test.command.write", move |envelope| {
            let meta = envelope.meta.clone().unwrap_or_default();
            *seeing.lock().unwrap() = Some((meta.shard, meta.shard_epoch));
            Ok(("meridian.v1.TakeShardReply".into(), Vec::new()))
        });
        assert_eq!(raise_epoch(&bus, "feed-1", &b).await.unwrap(), 7);
        let (request, stamped) = asked.lock().unwrap().pop().unwrap();
        assert_eq!(stamped, 0, "the take itself is sent under no epoch");
        assert_eq!(
            (
                request.instance.as_str(),
                request.shard,
                request.lease_transitions,
                request.holder.as_str(),
                request.previous_holder.as_str(),
                request.reason
            ),
            (
                "feed-1",
                0,
                // Its second take: `a` took it free from the launcher's
                // making (item 31), at 1.
                2,
                "b",
                "a",
                meridian_domain::v1::TakeoverReason::LeaseLapsed as i32
            )
        );
        // The Lease object's uid, as the API server holds it (c5a2 N5).
        let uid = api
            .leases
            .lock()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .uid
            .clone();
        assert!(!uid.is_empty());
        assert_eq!(request.lease_uid, uid);
        assert_eq!(bus.shard_stamped(), Some((0, 7)));
        bus.call("platform.test.command.write", "t", Vec::new(), None, None)
            .await
            .unwrap();
        assert_eq!(*seen.lock().unwrap(), Some((0, 7)));
        // A Lease nobody held: taken as released, from nobody.
        let fresh = Arc::new(Memory::default());
        let mut first = pod(&fresh, 1, "f");
        assert_eq!(first.look(start, 0).await, shard(0, &[]));
        let taken = first.taken().await.unwrap();
        // Made free (item 31), so its first take is its first transition.
        assert_eq!(
            (taken.transitions, taken.from.as_str(), taken.free),
            (1, "", true)
        );
    }

    /// Review c5a2 N5: a shard whose Lease was made again -- its
    /// transitions from 0 -- is taken at a new epoch: the take names the new
    /// Lease's uid, and the floor the old Lease's takes raised holds it no
    /// more.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_lease_made_again_is_taken_at_a_new_epoch() {
        let fence = Arc::new(meridian_changes::shard_fence::Memory::default());
        let old = meridian_changes::shard_fence::Taking {
            lease_transitions: 7,
            lease_uid: "the Lease before".into(),
            admit: None,
        };
        assert_eq!(fence.take("feed-1", 0, &old), Ok(7));
        let bus = lake_numbering(&fence);
        let api = Arc::new(Memory::default());
        let mut fresh = pod(&api, 1, "f");
        assert_eq!(fresh.look(Instant::now(), 0).await, shard(0, &[]));
        assert_eq!(take_epoch(&bus, "feed-1", &mut fresh).await, Ok(8));
    }

    /// A bus whose lake numbers takes by `fence`, as the control writer does
    /// (refusing a take below the transitions seen), for the tests below.
    fn lake_numbering(fence: &Arc<meridian_changes::shard_fence::Memory>) -> meridian_bus::Bus {
        use prost::Message as _;
        let bus = meridian_bus::Bus::single(
            "feed-1",
            Arc::new(meridian_bus::MemoryBackend::new()),
            Arc::new(meridian_clock::ManualClock::at(1)),
        );
        let numbering = Arc::clone(fence);
        bus.serve(meridian_lake::service::TAKE_SHARD, move |envelope| {
            let request =
                meridian_domain::v1::TakeShardRequest::decode(&envelope.payload[..]).unwrap();
            let taking = meridian_changes::shard_fence::Taking {
                lease_transitions: request.lease_transitions,
                lease_uid: request.lease_uid,
                admit: None,
            };
            let epoch = numbering
                .take(&request.instance, request.shard, &taking)
                .map_err(|refused| format!("fenced: {refused:?}"))?;
            Ok((
                "meridian.v1.TakeShardReply".into(),
                meridian_domain::v1::TakeShardReply { epoch }.encode_to_vec(),
            ))
        });
        bus
    }

    /// Review c5a M2 (paused clock): a pod whose take stalls past its
    /// Lease's lapse -- its bus slow to set up, nothing renewing -- never
    /// fences the pod that took the lapsed Lease after it: it reads its
    /// Lease again before it asks, finds the other holder, and stops; and
    /// the lake would refuse its take under fewer transitions anyway.
    #[tokio::test(start_paused = true)]
    async fn a_pod_whose_take_stalled_past_its_lapse_never_fences_the_holder_that_took_it_since() {
        let api = Arc::new(Memory::default());
        let (mut b, mut c) = (pod(&api, 1, "b"), pod(&api, 1, "c"));
        assert_eq!(b.look(clock(), 0).await, shard(0, &[]));
        assert_eq!(c.look(clock(), 0).await, Stand::Standby);
        // b's take stalls; nothing renews its Lease.
        tokio::time::sleep(secs(7.0)).await;
        assert_eq!(c.look(clock(), 0).await, shard(0, &[]));
        let fence = Arc::new(meridian_changes::shard_fence::Memory::default());
        let bus = lake_numbering(&fence);
        let c_epoch = take_epoch(&bus, "feed-1", &mut c).await.unwrap();
        // b's take, late.
        let late = take_epoch(&bus, "feed-1", &mut b).await;
        assert!(late.is_err(), "the late take was answered: {late:?}");
        // The holder that took the Lease since is not fenced.
        let stamp = meridian_changes::shard_fence::ShardStamp::of("feed-1", 0, c_epoch);
        assert_eq!(fence.check(&[stamp.as_ref()]), vec![Ok(())]);
        assert_eq!(fence.epoch("feed-1", 0), c_epoch);
    }

    /// Review c5a L3: a take names whom it took the shard from -- the pod
    /// that released it, as this pod last saw it held; itself, where it
    /// takes back its own name's Lease after its container restarted -- and
    /// whether the Lease was free.
    #[tokio::test(start_paused = true)]
    async fn a_take_names_whom_it_took_the_shard_from() {
        // Released by a: b takes it free, from a.
        let api = Arc::new(Memory::default());
        let (mut a, mut b) = (pod(&api, 1, "a"), pod(&api, 1, "b"));
        assert_eq!(a.look(clock(), 0).await, shard(0, &[]));
        assert_eq!(b.look(clock(), 0).await, Stand::Standby);
        a.release(0).await;
        assert_eq!(b.look(clock(), 0).await, shard(0, &[]));
        let taken = b.taken().await.unwrap();
        assert_eq!((taken.from.as_str(), taken.free), ("a", true));
        // A container restarted in its pod: its own name's Lease, taken back
        // from itself once it lapsed, not free.
        let api = Arc::new(Memory::default());
        let mut x = pod(&api, 1, "x");
        assert_eq!(x.look(clock(), 0).await, shard(0, &[]));
        drop(x);
        tokio::time::sleep(secs(7.0)).await;
        let mut again = pod(&api, 1, "x");
        assert_eq!(again.look(clock(), 0).await, shard(0, &[]));
        let taken = again.taken().await.unwrap();
        assert_eq!((taken.from.as_str(), taken.free), ("x", false));
        // Lapsed from another: from it, not free.
        let api = Arc::new(Memory::default());
        let (mut p, mut q) = (pod(&api, 1, "p"), pod(&api, 1, "q"));
        assert_eq!(p.look(clock(), 0).await, shard(0, &[]));
        assert_eq!(q.look(clock(), 0).await, Stand::Standby);
        tokio::time::sleep(secs(7.0)).await;
        assert_eq!(q.look(clock(), 0).await, shard(0, &[]));
        let taken = q.taken().await.unwrap();
        assert_eq!((taken.from.as_str(), taken.free), ("p", false));
    }

    /// Review c5a H1, the book: a book database that lost a take's raise --
    /// a new timeline, or a database added later -- is raised again to the
    /// highest epoch the lake's records of the takes name, read a page at a
    /// time from the lake's control partition; an old holder's write is
    /// then refused there.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_book_that_lost_a_raise_is_raised_again_from_the_lake_s_records() {
        use prost::Message as _;
        let bus = Arc::new(meridian_bus::Bus::single(
            "book-1",
            Arc::new(meridian_bus::MemoryBackend::new()),
            Arc::new(meridian_clock::ManualClock::at(1)),
        ));
        // The lake's control partition: two takes of shard 1, one of shard 0,
        // and another record; a page of two at a time.
        let change = |sequence: u64, kind: &str, body: Vec<u8>| meridian_changes::Change {
            partition: meridian_changes::CONTROL.into(),
            sequence,
            kind: kind.into(),
            body,
            ..Default::default()
        };
        let took = |shard, epoch| {
            meridian_domain::v1::ShardTookOver {
                instance: "feed-1".into(),
                shard,
                epoch,
                ..Default::default()
            }
            .encode_to_vec()
        };
        let kind = meridian_lake::store::CONTROL_SHARD_TOOK_OVER;
        let log = vec![
            change(1, kind, took(1, 2)),
            change(2, "meridian.v1.PartitionOwner", Vec::new()),
            change(3, kind, took(1, 3)),
            change(4, kind, took(0, 1)),
        ];
        bus.serve(
            &meridian_changes::read_changes_topic("lake"),
            move |envelope| {
                let asked =
                    meridian_changes::ReadChangesRequest::decode(&envelope.payload[..]).unwrap();
                let page = meridian_changes::ReadChangesReply {
                    changes: log
                        .iter()
                        .filter(|c| c.sequence > asked.after_sequence)
                        .take(2)
                        .cloned()
                        .collect(),
                    head: 4,
                    ..Default::default()
                };
                Ok(("meridian.v1.ReadChangesReply".into(), page.encode_to_vec()))
            },
        );
        // The book's database, its raise to 3 lost: it holds 1.
        let book = Arc::new(meridian_changes::shard_fence::Memory::default());
        book.raise_to("feed-1", 1, 1);
        let raising = Arc::clone(&book);
        let shards = raise_book_to_records(&bus, move |instance, shard, epoch| {
            Ok(raising.raise_to(instance, shard, epoch))
        })
        .await
        .unwrap();
        assert_eq!(shards, 2);
        assert_eq!((book.epoch("feed-1", 1), book.epoch("feed-1", 0)), (3, 1));
        let stale = meridian_changes::shard_fence::ShardStamp::of("feed-1", 1, 2);
        assert!(book.check(&[stale.as_ref()])[0].is_err());
    }

    /// A shard told to stop releases its Lease, and the standby holds it at
    /// its next look.
    #[tokio::test(start_paused = true)]
    async fn a_shard_told_to_stop_releases_its_lease_to_the_standby() {
        let store = Arc::new(Memory::default());
        let (stopper, stop) = watch::channel(false);
        let a = serving(on(&Link::to(&store), 1, "a"), &stop);
        tokio::time::sleep(secs(0.1)).await;
        let (_c_stopper, c_stop) = watch::channel(false);
        let c = standing_by(on(&Link::to(&store), 1, "c"), &c_stop);
        tokio::time::sleep(secs(5.0)).await;
        stopper.send(true).unwrap();
        let (released_at, kept) = a.await.unwrap();
        assert_eq!(kept, Kept::Released);
        let (held_at, _, _) = c.await.unwrap().expect("the standby took the shard");
        assert!(held_at.duration_since(released_at) <= secs(1.0));
    }
}
