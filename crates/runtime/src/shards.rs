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
//! **No epoch fences a plugin**, as a store's fences its writer. So a shard
//! that cannot renew does not write on as a store's member does: it stops
//! once it has gone the lease's duration less one renewal unrenewed, before
//! any standby can see the lease lapse (client-go's renew deadline), and so
//! does one that finds another holder. Its process ends, its plugin's
//! stream with it, and its pod comes back as the standby.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::lease::{Elector, Leases, Settings, Standing};

/// The instance's shard count, set by the launcher on a launch with
/// several shards or a standby pod (1 for a standby alone). Unset: the
/// instance's one pod, which takes no Lease.
pub const SHARDS_ENV: &str = "MERIDIAN_PLUGIN_SHARDS";

/// The most shards an instance runs (`PluginDeclaration.most_shards`).
pub const MOST: u32 = 64;

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

/// One pod's view of its instance's shard Leases.
pub struct ShardSet<L: Leases> {
    electors: Vec<Elector<Arc<L>>>,
    mine: Option<usize>,
    /// Which other shards were live when each was last read.
    live: Vec<bool>,
}

impl<L: Leases> ShardSet<L> {
    pub fn new(
        api: Arc<L>,
        instance: &str,
        count: u32,
        holder: &str,
        renew: Duration,
        duration: Duration,
    ) -> Result<Self, String> {
        let mut electors = Vec::with_capacity(count as usize);
        for shard in 0..count.max(1) {
            let settings = Settings {
                name: lease_name(instance, shard),
                holder: holder.to_string(),
                renew,
                duration,
            };
            settings.check()?;
            electors.push(Elector::new(Arc::clone(&api), settings));
        }
        let live = vec![false; electors.len()];
        Ok(Self {
            electors,
            mine: None,
            live,
        })
    }

    /// How often to look: a standby twice a renewal, as a store's does; a
    /// shard every renewal.
    pub fn look_every(&self) -> Duration {
        let settings = self.electors[0].settings();
        match self.mine {
            None => settings.look_every(),
            Some(_) => settings.renew,
        }
    }

    /// One look. A pod holding no shard takes the first Lease free or
    /// lapsed; a shard renews its own and reads the others'.
    pub async fn look(&mut self, now: Instant, wall_ns: i64) -> Stand {
        let Some(mine) = self.mine else {
            for (shard, elector) in self.electors.iter_mut().enumerate() {
                match elector.look(now, wall_ns).await {
                    Ok(Standing::Held) => {
                        self.mine = Some(shard);
                        break;
                    }
                    Ok(Standing::HeldBy(_)) => {}
                    Err(failed) => {
                        tracing::warn!(shard, %failed, "a shard's lease could not be read");
                    }
                }
            }
            return match self.mine {
                // Its first look as a shard reads the others next time.
                Some(mine) => self.standing(mine),
                None => Stand::Standby,
            };
        };
        let elector = &mut self.electors[mine];
        match elector.look(now, wall_ns).await {
            Ok(Standing::Held) => {}
            Ok(Standing::HeldBy(other)) => {
                return Stand::Lost(format!(
                    "{} is held by {}",
                    elector.settings().name,
                    if other.is_empty() { "nobody" } else { &other }
                ));
            }
            Err(failed) => {
                let deadline = elector.settings().renew_deadline();
                let since = elector
                    .renewed_at()
                    .map_or(deadline, |at| now.saturating_duration_since(at));
                if since >= deadline {
                    return Stand::Lost(format!(
                        "{} could not be renewed for {} s, the lease's duration less one \
                         renewal: {failed}",
                        elector.settings().name,
                        since.as_secs()
                    ));
                }
                tracing::warn!(shard = mine, %failed, "the shard's lease was not renewed; trying again");
            }
        }
        for (shard, elector) in self.electors.iter_mut().enumerate() {
            if shard == mine {
                continue;
            }
            match elector.watch(now).await {
                Ok(holder) => self.live[shard] = holder.is_some(),
                // Not read: as it was last read.
                Err(failed) => {
                    tracing::debug!(shard, %failed, "another shard's lease could not be read")
                }
            }
        }
        self.standing(mine)
    }

    fn standing(&self, mine: usize) -> Stand {
        Stand::Shard {
            mine: mine as u32,
            others: (0..self.electors.len())
                .filter(|shard| *shard != mine && self.live[*shard])
                .map(|shard| shard as u32)
                .collect(),
        }
    }

    /// Give this pod's shard up: a planned stop hands it to the standby at
    /// once.
    pub async fn release(&mut self, wall_ns: i64) {
        if let Some(mine) = self.mine {
            self.electors[mine].release(wall_ns).await;
        }
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
    }

    impl Memory {
        fn up(&self) -> Result<(), String> {
            if self.down.load(Ordering::SeqCst) {
                Err("the API server did not answer".into())
            } else {
                Ok(())
            }
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
            assert_eq!(b.look(start, 0).await, shard(1, &[]));
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
            assert_eq!(again.look(now, 0).await, shard(1, &[]));
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
}
