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
//! finds another holder. Its process ends, its plugin's stream with it, and
//! its pod comes back as the standby. This is a property of the role, never
//! a setting.

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
    instance: String,
    electors: Vec<Shared<L>>,
    settings: Settings,
    mine: Option<usize>,
    /// Which other shards were live when each was last read.
    live: Vec<bool>,
    /// Whether its shard writes on unrenewed, fenced by its epoch
    /// ([`ShardSet::writing_on`]); else it stops at its renew deadline.
    writes_on: bool,
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
        let mut electors = Vec::with_capacity(count as usize);
        let mut first = None;
        for shard in 0..count.max(1) {
            let settings = Settings {
                name: lease_name(instance, shard),
                holder: holder.to_string(),
                renew,
                duration,
            };
            settings.check()?;
            first.get_or_insert_with(|| settings.clone());
            electors.push(Arc::new(tokio::sync::Mutex::new(
                Elector::new(Arc::clone(&api), settings).of_a_holder_that_stops(clock),
            )));
        }
        let live = vec![false; electors.len()];
        Ok(Self {
            instance: instance.to_string(),
            electors,
            settings: first.expect("at least one shard"),
            mine: None,
            live,
            writes_on: false,
        })
    }

    /// A shard fenced by its epoch, which writes on through an API server
    /// outage (step 3d, checkpoint 5, item 32; [`writes_on`]): a renewal
    /// that fails or hangs never ends it. It is lost only when its Lease
    /// shows another holder -- or, outside this set, when the lake or the
    /// book refuses one of its writes as fenced.
    pub fn writing_on(mut self) -> Self {
        self.writes_on = true;
        self
    }

    /// The shard this pod holds, the Lease's transitions as it took it --
    /// the floor of the epoch the lake raises (item 32) -- whom it took it
    /// from, and this pod's holder name. None before a take.
    pub async fn taken(&self) -> Option<(u32, u64, Option<Option<String>>, String)> {
        let mine = self.mine?;
        let elector = self.electors[mine].lock().await;
        Some((
            mine as u32,
            elector.transitions()?,
            elector.took_from(),
            elector.settings().holder.clone(),
        ))
    }

    /// How often to look: a standby twice a renewal, as a store's does; a
    /// shard every renewal.
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
            // One not read is held live, as the others of a running
            // instance are, until it is read.
            for (shard, held) in read(&self.others(mine), now).await {
                self.live[shard] = held.unwrap_or(true);
            }
            return self.standing(mine);
        };
        let looked = self.electors[mine].lock().await.look(now, wall_ns).await;
        match looked {
            Ok(Standing::Held) => {}
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
                _ = tokio::time::sleep(self.look_every()) => {}
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
            let stop_by = tokio::time::Instant::from_std(renewed + deadline);
            // Fenced, it writes on: a renewal waits one renewal at most and
            // is tried again; the clock never ends it.
            let give_up_by = if writes_on {
                tokio::time::Instant::now() + renew
            } else {
                stop_by
            };
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

    /// Give this pod's shard up: a planned stop hands it to the standby at
    /// once.
    pub async fn release(&mut self, wall_ns: i64) {
        if let Some(mine) = self.mine {
            self.electors[mine].lock().await.release(wall_ns).await;
        }
    }
}

/// How long a take waits for the lake: its every writer group's
/// confirmation, with the way there and back; the Lease is renewed only
/// after it ([`ShardSet::renew_now`]), inside the lease's lapse.
pub const TAKE_SHARD_WITHIN: Duration = Duration::from_secs(3);

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
    let (shard, transitions, took_from, holder) = set
        .taken()
        .await
        .ok_or("this pod holds no shard to raise the epoch of")?;
    let previous_holder = took_from.flatten().unwrap_or_default();
    let reason = if previous_holder.is_empty() {
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

/// The reading of the other shards' Leases, ended when it is dropped.
struct Reading(tokio::task::JoinHandle<()>);

impl Drop for Reading {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::{Record, Wrote};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    /// The namespace's Leases, as the API server keeps them: every write
    /// conditional on the version read. `down` answers nothing.
    #[derive(Default)]
    struct Memory {
        leases: Mutex<HashMap<String, Record>>,
        down: AtomicBool,
        /// When each Lease was last written.
        written: Mutex<HashMap<String, tokio::time::Instant>>,
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
        fn holder(&self, name: &str) -> String {
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
            leases.insert(name.to_string(), record);
            self.wrote(name);
            Ok(Wrote::Done(next))
        }
    }

    const RENEW: Duration = Duration::from_secs(2);
    const LASTS: Duration = Duration::from_secs(6);

    fn pod(api: &Arc<Memory>, count: u32, holder: &str) -> ShardSet<Memory> {
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
    ) -> tokio::task::JoinHandle<(At, Kept)> {
        let (stop, fence, epoch) = (stop.clone(), Arc::clone(fence), Arc::clone(epoch));
        tokio::spawn(async move {
            let (set, mine, _) = set
                .writing_on()
                .take(stop.clone())
                .await
                .unwrap()
                .expect("a shard");
            let (_, transitions, _, _) = set.taken().await.unwrap();
            *epoch.lock().unwrap() = fence.take("feed-1", mine, transitions);
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
        let (_stopper, stop) = watch::channel(false);
        let a = serving_fenced(on(&to_a, 1, "a"), &stop, &fence, &a_epoch);
        tokio::time::sleep(secs(0.1)).await;
        let c = serving_fenced(on(&to_c, 1, "c"), &stop, &fence, &c_epoch);
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
                1,
                "b",
                "a",
                meridian_domain::v1::TakeoverReason::LeaseLapsed as i32
            )
        );
        assert_eq!(bus.shard_stamped(), Some((0, 7)));
        bus.call("platform.test.command.write", "t", Vec::new(), None, None)
            .await
            .unwrap();
        assert_eq!(*seen.lock().unwrap(), Some((0, 7)));
        // A Lease nobody held: taken as released, from nobody.
        let fresh = Arc::new(Memory::default());
        let mut first = pod(&fresh, 1, "f");
        assert_eq!(first.look(start, 0).await, shard(0, &[]));
        let (_, transitions, from, _) = first.taken().await.unwrap();
        assert_eq!((transitions, from), (0, Some(None)));
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
