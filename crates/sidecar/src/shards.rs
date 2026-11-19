//! A plugin instance's shards, at one shard's sidecar (step 3d; spec/a-
//! deployment-scales-out-and-fails-over-by-configuration section 7; ruling
//! 1056, question 11).
//!
//! A shard is a pod of one plugin instance: the instance's datasets,
//! licence, settings and credential, and its own connection to the vendor.
//! Each shard's sidecar holds one of the instance's Leases,
//! `<instance>-shard-<n>`, and reads which of the others are held: the live
//! shards. The subjects are shared among them by rendezvous hashing
//! (highest random weight): every shard scores each subject against each
//! live shard the same way, and the shard scoring highest keeps it. So a
//! shard lost or added moves about one subject in N, and no shard needs to
//! be told another's share.
//!
//! What is shared is what the lake wants of the instance (W10.7): each
//! shard hears every want, as every sidecar of the instance does, and hands
//! its plugin a want naming its own subjects alone -- the same want ID, so
//! the SDK's standing want set follows with no plugin code. A want none of
//! whose subjects are this shard's is not handed on; one whose share here
//! became empty is handed on as withdrawn. When the live shards change,
//! each standing want kept is shared again at once, so a lost shard's
//! subjects are another's within the lease's lapse, not at the lake's next
//! redelivery. A subject briefly held by two shards records the same rows,
//! which the lake keeps once by version.
//!
//! **A taker's share at once** (ruling 184, option B; item 33): a taker's
//! keep starts empty, and its plugin's stream opens only after its take. So
//! the lake, on a take, delivers the instance's standing wants again at
//! once, and the taker's sidecar listens for them from before it asks to
//! take: it holds what it hears of its instance's wants, bounded, until its
//! plugin's stream opens, and hands its share then through the stream's own
//! path. Nothing is kept past that hand-over.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::{Mutex, RwLock};
use std::time::Duration;

use meridian_bus::{Bus, Delivery, MessageMeta, Subscription};
use meridian_domain::v1::{ObservationsWantedEvent, WantWithdrawnEvent};
use prost::Message as _;
use tokio::sync::{oneshot, watch};

/// A store refused one of this sidecar's writes as fenced (step 3d,
/// checkpoint 5, item 32: `REFUSAL_REASON_SHARD_FENCED`): another pod took
/// its shard. Said once by the refusal's path; the sidecar waits on it and
/// ends its serving, as on a lost Lease, and stands by again in its process
/// (item 49, ruled 2026-11-16).
static FENCED: std::sync::LazyLock<watch::Sender<bool>> =
    std::sync::LazyLock::new(|| watch::channel(false).0);

/// Say this sidecar's shard was fenced: its plugin has its refusal, and
/// its serving ends.
pub fn say_fenced(words: &str) {
    if !*FENCED.borrow() {
        tracing::error!(words, "a store refused a write as fenced: this shard's epoch is no longer the instance's; standing by again");
    }
    FENCED.send_replace(true);
}

/// A new serving begins unfenced: a refusal heard was a shard's this
/// sidecar has since stood by again for (item 49).
pub fn clear_fenced() {
    FENCED.send_replace(false);
}

/// Whether a store has refused a write as fenced.
pub fn was_fenced() -> bool {
    *FENCED.borrow()
}

/// Wait until a store refuses a write as fenced.
pub async fn fenced() {
    let mut seen = FENCED.subscribe();
    if seen.wait_for(|fenced| *fenced).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// The most standing wants a shard keeps to share again when the live
/// shards change: the oldest forgotten first, and shared again at the
/// lake's redelivery. As many one-off wants are kept beside them, for what
/// of each was handed on, and never in their place.
const WANTS_KEPT: usize = 4_096;

/// The most withdrawals a shard remembers (review c5z F2): as many as it
/// keeps wants of both kinds, the oldest forgotten first. A want's ID is
/// never the lake's again once withdrawn, and the lake delivers no want
/// after its withdrawal but in a moment's race, so a want heard after it is
/// heard while it is remembered.
const WITHDRAWALS_KEPT: usize = 2 * WANTS_KEPT;

/// The topics of the two rows a data plugin hears its instance's wants by
/// (W10.7), as the lake publishes them.
pub(crate) const WANTED_TOPIC: &str = "platform.lake.event.observations-wanted";
pub(crate) const WITHDRAWN_TOPIC: &str = "platform.lake.event.want-withdrawn";

/// The most wants a taker holds for its plugin's stream (ruling 184): what
/// the stream's queue takes at its opening, so the whole hold is handed at
/// once. Past it a want is left out and the plugin told of a loss; the
/// lake's redelivery brings it. As many withdrawals are remembered.
const HOLD_MOST: usize = crate::receive::QUEUE;

/// Whether a plugin of `roles` hears its instance's wants (W10.7: the
/// `data` role), by the contract compiled into this runtime: only such a
/// shard holds them at its take.
pub fn hears_wants(roles: &[String]) -> bool {
    let contract = crate::grants::Contract::embedded();
    !contract.roles_hearing(roles, WANTED_TOPIC).is_empty()
}

/// One shard's view of its instance's shards.
pub struct Shards {
    /// The shard this sidecar is: the `n` of the Lease it holds.
    mine: u32,
    /// How many shards its instance runs, as its launch set them (1 for a
    /// standby pod alone): what its plugin is told at registration with
    /// [`Shards::mine`] (item 30).
    count: u32,
    live: RwLock<BTreeSet<u32>>,
    changed: watch::Sender<u64>,
    wants: Mutex<Wants>,
    /// The take's hold of its instance's wants, until the plugin's stream
    /// asks for it (ruling 184): asked once, by the stream, with where to
    /// answer.
    hold: Mutex<Option<oneshot::Sender<oneshot::Sender<Held>>>>,
    /// The take answered: the hold's bound counted from then.
    hold_from: Mutex<Option<oneshot::Sender<()>>>,
    /// Called once a want's part is worked out and before it is kept.
    #[cfg(test)]
    between: Mutex<Option<Box<dyn Fn() + Send>>>,
    /// Called once a stream has been handed the take's hold, and before it
    /// shares its wants again on the live shards' changes.
    #[cfg(test)]
    handed_over: Mutex<Option<Box<dyn Fn() + Send>>>,
}

#[derive(Default)]
struct Wants {
    /// Each want heard, whole, and the subjects of it handed to the plugin.
    kept: HashMap<String, (ObservationsWantedEvent, BTreeSet<String>)>,
    /// The standing wants in the order heard, and the one-off wants: each
    /// forgets its own oldest, so a burst of one-off reads pushes no
    /// standing want out.
    standing: VecDeque<String>,
    once: VecDeque<String>,
    /// The latest wants withdrawn, and the order they were heard in (review
    /// c5z F2): a want and its withdrawal are two topics, heard in either
    /// order, and a want heard after its withdrawal is never kept or
    /// handed, since nothing would withdraw it again.
    gone: HashSet<String>,
    gone_order: VecDeque<String>,
}

impl Wants {
    /// `want_id` withdrawn: remembered, the oldest past
    /// [`WITHDRAWALS_KEPT`] forgotten.
    fn gone(&mut self, want_id: String) {
        if self.gone.insert(want_id.clone()) {
            self.gone_order.push_back(want_id);
            while self.gone_order.len() > WITHDRAWALS_KEPT {
                if let Some(oldest) = self.gone_order.pop_front() {
                    self.gone.remove(&oldest);
                }
            }
        }
    }
}

/// What a taker heard of its instance's wants from before its take until
/// its plugin's stream opened (ruling 184), handed to that stream.
pub(crate) struct Held {
    /// The wants, in the order first heard, each the latest heard under its
    /// ID with the message it came in; none a withdrawal heard in the window
    /// named.
    pub(crate) wants: Vec<(ObservationsWantedEvent, MessageMeta)>,
    /// The two subscriptions, open since before the take: the stream
    /// forwards from them, so nothing heard since is missed or heard twice.
    pub(crate) wanted: Subscription,
    pub(crate) withdrawn: Subscription,
    /// Some were left out: the hold was full, one did not read, or the bus
    /// dropped some.
    pub(crate) lost: bool,
    /// The withdrawals heard in the window: the shard remembers them as it
    /// hands the hold over (review c5z F2), since their wants may still wait
    /// in the subscriptions the stream forwards from.
    gone: HashSet<String>,
}

/// The hold's wants, by ID, and the withdrawals heard in it.
#[derive(Default)]
struct Holding {
    order: Vec<String>,
    wants: HashMap<String, (ObservationsWantedEvent, MessageMeta)>,
    gone: HashSet<String>,
    lost: bool,
}

impl Holding {
    /// A want heard: held where it is the instance's, the latest under its
    /// ID once; never one withdrawn in the window, whichever was heard
    /// first.
    fn want(&mut self, instance: &str, heard: &Delivery) {
        let Ok(want) = ObservationsWantedEvent::decode(&heard.envelope.payload[..]) else {
            self.lost = true;
            return;
        };
        if meridian_domain::lake::instance_of(&want.dataset) != Some(instance)
            || self.gone.contains(&want.want_id)
        {
            return;
        }
        let meta = heard.envelope.meta.clone().unwrap_or_default();
        if let Some(held) = self.wants.get_mut(&want.want_id) {
            *held = (want, meta);
        } else if self.wants.len() < HOLD_MOST {
            self.order.push(want.want_id.clone());
            self.wants.insert(want.want_id.clone(), (want, meta));
        } else {
            self.lost = true;
        }
    }

    /// A withdrawal heard: the want it names never handed. False where more
    /// were heard than a hold remembers: the hold then ends, handing
    /// nothing, rather than forget one.
    fn withdrawn(&mut self, instance: &str, heard: &Delivery) -> bool {
        let Ok(gone) = WantWithdrawnEvent::decode(&heard.envelope.payload[..]) else {
            self.lost = true;
            return true;
        };
        if meridian_domain::lake::instance_of(&gone.dataset) != Some(instance) {
            return true;
        }
        if self.wants.remove(&gone.want_id).is_some() {
            self.order.retain(|held| held != &gone.want_id);
        }
        if self.gone.len() >= HOLD_MOST {
            return false;
        }
        self.gone.insert(gone.want_id);
        true
    }
}

/// A take's hold, until the stream asks for it, `within` passes, the shard
/// is fenced, or the bus ends. `within` counts from the hold's start, and
/// again from the take's answer when `from_take` tells of it. Asked, it
/// answers what it holds with its two subscriptions; otherwise it ends, and
/// what it held is forgotten.
async fn holding(
    instance: String,
    mut wanted: Subscription,
    mut withdrawn: Subscription,
    mut asked: oneshot::Receiver<oneshot::Sender<Held>>,
    mut from_take: oneshot::Receiver<()>,
    within: Duration,
) {
    enum Woke {
        Asked(oneshot::Sender<Held>),
        Ended,
        Taken,
        Want(Delivery),
        Withdrawn(Delivery),
    }
    let mut held = Holding::default();
    let until = tokio::time::sleep(within);
    tokio::pin!(until);
    let mut counted_from_take = false;
    loop {
        let woke = tokio::select! {
            biased;
            asked = &mut asked => match asked {
                Ok(answer) => Woke::Asked(answer),
                Err(_) => Woke::Ended,
            },
            _ = fenced() => Woke::Ended,
            _ = &mut until => Woke::Ended,
            _ = &mut from_take, if !counted_from_take => Woke::Taken,
            heard = withdrawn.recv() => heard.map_or(Woke::Ended, Woke::Withdrawn),
            heard = wanted.recv() => heard.map_or(Woke::Ended, Woke::Want),
        };
        match woke {
            Woke::Asked(answer) => {
                let lost = held.lost || wanted.dropped() > 0 || withdrawn.dropped() > 0;
                let mut wants = held.wants;
                let _ = answer.send(Held {
                    wants: held
                        .order
                        .iter()
                        .filter_map(|id| wants.remove(id))
                        .collect(),
                    wanted,
                    withdrawn,
                    lost,
                    gone: held.gone,
                });
                return;
            }
            // The take answered (or will not be): the bound counted from
            // now, once.
            Woke::Taken => {
                counted_from_take = true;
                until.as_mut().reset(tokio::time::Instant::now() + within);
            }
            Woke::Ended => {
                tracing::debug!(
                    instance,
                    held = held.order.len(),
                    "a take's hold of its wants ended before its plugin's stream opened"
                );
                return;
            }
            Woke::Want(heard) => held.want(&instance, &heard),
            Woke::Withdrawn(heard) => {
                if !held.withdrawn(&instance, &heard) {
                    tracing::warn!(
                        instance,
                        "a take heard more withdrawals than it holds: it hands nothing, and the lake's redelivery serves"
                    );
                    return;
                }
            }
        }
    }
}

/// What a shard's plugin is handed of a want heard.
#[derive(Debug, Clone, PartialEq)]
pub enum Handed {
    /// The want, naming this shard's subjects alone.
    Part(Box<ObservationsWantedEvent>),
    /// Nothing of it is this shard's any longer, and some was.
    Withdrawn(WantWithdrawnEvent),
    /// Nothing to hand on: none of it is this shard's, and none was.
    Nothing,
}

/// A subject's weight at a shard: FNV-1a over the subject and the shard,
/// then mixed (splitmix64's finalizer), spelled out so every release of the
/// sidecar scores alike.
fn weight(subject: &str, shard: u32) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in subject
        .as_bytes()
        .iter()
        .copied()
        .chain([0xff])
        .chain(shard.to_be_bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash ^= hash >> 30;
    hash = hash.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    hash ^= hash >> 27;
    hash = hash.wrapping_mul(0x94d0_49bb_1331_11eb);
    hash ^ (hash >> 31)
}

/// The shard of `live` that keeps `subject`: the one it weighs most at.
pub fn keeper(subject: &str, live: &BTreeSet<u32>) -> Option<u32> {
    live.iter()
        .copied()
        .max_by_key(|shard| (weight(subject, *shard), *shard))
}

impl Shards {
    /// This sidecar as shard `mine`, alone until it reads the others.
    pub fn new(mine: u32) -> Self {
        Self {
            mine,
            count: mine + 1,
            live: RwLock::new(BTreeSet::from([mine])),
            changed: watch::channel(0).0,
            wants: Mutex::default(),
            hold: Mutex::default(),
            hold_from: Mutex::default(),
            #[cfg(test)]
            between: Mutex::default(),
            #[cfg(test)]
            handed_over: Mutex::default(),
        }
    }

    pub fn mine(&self) -> u32 {
        self.mine
    }

    /// Of `count` shards, as the instance's launch set them (item 30).
    pub fn of(mut self, count: u32) -> Self {
        self.count = count.max(self.mine + 1);
        self
    }

    /// How many shards the instance runs (item 30).
    pub fn count(&self) -> u32 {
        self.count
    }

    /// The live shards, this one among them.
    pub fn live(&self) -> BTreeSet<u32> {
        self.live.read().expect("shards poisoned").clone()
    }

    /// The live shards as the Leases now say; this one is always among
    /// them, since this sidecar serves only while it holds its own.
    pub fn set_live(&self, others: impl IntoIterator<Item = u32>) {
        let mut now: BTreeSet<u32> = others.into_iter().collect();
        now.insert(self.mine);
        let mut live = self.live.write().expect("shards poisoned");
        if *live != now {
            tracing::info!(
                shard = self.mine,
                live = ?now,
                was = ?*live,
                "the instance's live shards changed; its subjects are shared again"
            );
            *live = now;
            drop(live);
            self.changed.send_modify(|version| *version += 1);
        }
    }

    /// Told each time the live shards change.
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    /// From before its take (ruling 184): hear the instance's wants and
    /// their withdrawals on `bus`, as any of its sidecars hears them --
    /// subscribed, every message, sending nothing -- and hold them until
    /// its plugin's stream opens and asks ([`Shards::take_held`]), or
    /// `within` passes. A shard whose plugin hears no wants holds nothing
    /// ([`hears_wants`]); its caller asks only for one that does.
    pub fn hold_wants(&self, bus: &Bus, instance: &str, within: Duration) {
        let wanted = bus.subscribe_every(WANTED_TOPIC);
        let withdrawn = bus.subscribe_every(WITHDRAWN_TOPIC);
        let (ask, asked) = oneshot::channel();
        let (taken, from_take) = oneshot::channel();
        *self.hold.lock().expect("hold poisoned") = Some(ask);
        *self.hold_from.lock().expect("hold poisoned") = Some(taken);
        tokio::spawn(holding(
            instance.to_string(),
            wanted,
            withdrawn,
            asked,
            from_take,
            within,
        ));
    }

    /// The take answered: the hold's bound counted again from now (review
    /// c5z, a nit), so it covers the lake's redelivery of what the take
    /// delivered again, which the lake counts from its answer.
    pub fn hold_from_the_take(&self) {
        if let Some(taken) = self.hold_from.lock().expect("hold poisoned").take() {
            let _ = taken.send(());
        }
    }

    /// Run at a stream's hand-over, before it shares its wants again.
    #[cfg(test)]
    pub(crate) fn at_hand_over(&self, run: impl Fn() + Send + 'static) {
        *self.handed_over.lock().unwrap() = Some(Box::new(run));
    }

    #[cfg(test)]
    pub(crate) fn handed_over(&self) {
        if let Some(run) = self.handed_over.lock().unwrap().as_ref() {
            run();
        }
    }

    /// What the take held, for the plugin's stream opening now: once, while
    /// the hold lasts, and never for a shard fenced.
    pub(crate) async fn take_held(&self) -> Option<Held> {
        let ask = self.hold.lock().expect("hold poisoned").take()?;
        if was_fenced() {
            return None;
        }
        let (answer, answered) = oneshot::channel();
        ask.send(answer).ok()?;
        let mut held = answered.await.ok()?;
        // Remembered before the stream forwards from the hold's
        // subscriptions, where a want withdrawn in the window may still wait
        // (review c5z F2).
        let mut wants = self.wants.lock().expect("wants poisoned");
        for want_id in held.gone.drain() {
            wants.gone(want_id);
        }
        drop(wants);
        Some(held)
    }

    /// Whether `subject` is this shard's to keep.
    pub fn keeps(&self, subject: &str) -> bool {
        keeper(subject, &self.live.read().expect("shards poisoned")) == Some(self.mine)
    }

    fn part(&self, want: &ObservationsWantedEvent) -> (ObservationsWantedEvent, BTreeSet<String>) {
        let live = self.live();
        let mut part = want.clone();
        part.subjects
            .retain(|subject| keeper(&subject.entity_id, &live) == Some(self.mine));
        let subjects = part
            .subjects
            .iter()
            .map(|subject| subject.entity_id.clone())
            .collect();
        (part, subjects)
    }

    /// A want heard: kept whole, and its part here to hand on, as
    /// [`Shards::heard_then`] with every hand taken.
    pub fn heard(&self, want: ObservationsWantedEvent) -> Handed {
        let mut out = Handed::Nothing;
        self.heard_then(want, |handed| {
            out = handed;
            true
        });
        out
    }

    /// A want heard: kept whole, and its part here handed by `hand`, which
    /// says whether it was taken -- queued for the plugin, or nothing to
    /// queue. A part not taken is kept as not handed, so the next sharing
    /// or redelivery hands it (review c5u2 N1): never marked handed and
    /// lost.
    ///
    /// Its part is worked out, handed and kept under the one lock a reshare
    /// takes ([`Shards::shared_again_then`]): a change of the live shards
    /// between them would else be shared over every want but this one, kept
    /// after with its part as the shards were.
    ///
    /// A want whose withdrawal was heard first is neither kept nor handed
    /// (review c5z F2).
    pub fn heard_then(&self, want: ObservationsWantedEvent, hand: impl FnOnce(Handed) -> bool) {
        let mut wants = self.wants.lock().expect("wants poisoned");
        if wants.gone.contains(&want.want_id) {
            return;
        }
        let (part, subjects) = self.part(&want);
        #[cfg(test)]
        if let Some(between) = self.between.lock().unwrap().as_ref() {
            between();
        }
        let before = wants
            .kept
            .get(&want.want_id)
            .map(|(was, handed)| (was.standing, handed.clone()));
        let wants = &mut *wants;
        if before.as_ref().map(|(standing, _)| *standing) != Some(want.standing) {
            wants.standing.retain(|held| held != &want.want_id);
            wants.once.retain(|held| held != &want.want_id);
            let order = if want.standing {
                &mut wants.standing
            } else {
                &mut wants.once
            };
            order.push_back(want.want_id.clone());
            while order.len() > WANTS_KEPT {
                if let Some(oldest) = order.pop_front() {
                    wants.kept.remove(&oldest);
                }
            }
        }
        let was = before.map(|(_, handed)| handed).unwrap_or_default();
        let (want_id, dataset) = (want.want_id.clone(), want.dataset.clone());
        let handed = if !subjects.is_empty() {
            Handed::Part(Box::new(part))
        } else if !was.is_empty() {
            Handed::Withdrawn(WantWithdrawnEvent {
                want_id: want_id.clone(),
                dataset,
            })
        } else {
            Handed::Nothing
        };
        let now = if hand(handed) { subjects } else { was };
        wants.kept.insert(want_id, (want, now));
    }

    /// A want the lake withdrew: forgotten, and handed on only where some
    /// of it was this shard's; remembered as withdrawn, so the want heard
    /// after it is never kept (review c5z F2).
    pub fn withdrawn(&self, want_id: &str) -> bool {
        let mut wants = self.wants.lock().expect("wants poisoned");
        wants.gone(want_id.to_string());
        wants.standing.retain(|held| held != want_id);
        wants.once.retain(|held| held != want_id);
        wants
            .kept
            .remove(want_id)
            .is_some_and(|(_, handed)| !handed.is_empty())
    }

    /// The standing wants whose part here changed since each was last
    /// handed on, shared again over the live shards as they are now, as
    /// [`Shards::shared_again_then`] with every hand taken.
    pub fn shared_again(&self) -> Vec<Handed> {
        let mut again = Vec::new();
        self.shared_again_then(|handed| {
            again.push(handed);
            true
        });
        again
    }

    /// The standing wants whose part here changed since each was last
    /// handed on, each handed by `hand` over the live shards as they are
    /// now; a part `hand` did not take stays as it was handed before, to be
    /// shared at the next change or redelivery (review c5u2 N1).
    pub fn shared_again_then(&self, mut hand: impl FnMut(Handed) -> bool) {
        let mut wants = self.wants.lock().expect("wants poisoned");
        let ids: Vec<String> = wants.standing.iter().cloned().collect();
        for id in ids {
            let Some((want, handed)) = wants.kept.get(&id) else {
                continue;
            };
            if !want.standing {
                continue;
            }
            let (part, subjects) = self.part(want);
            if &subjects == handed {
                continue;
            }
            let dataset = want.dataset.clone();
            let taken = hand(if subjects.is_empty() {
                Handed::Withdrawn(WantWithdrawnEvent {
                    want_id: id.clone(),
                    dataset,
                })
            } else {
                Handed::Part(Box::new(part))
            });
            if taken {
                if let Some(kept) = wants.kept.get_mut(&id) {
                    kept.1 = subjects;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use meridian_domain::v1::SubjectRef;

    fn want(id: &str, subjects: &[&str], standing: bool) -> ObservationsWantedEvent {
        ObservationsWantedEvent {
            want_id: id.into(),
            dataset: "feed:prices".into(),
            subjects: subjects
                .iter()
                .map(|entity| SubjectRef {
                    entity_id: (*entity).into(),
                    ..Default::default()
                })
                .collect(),
            standing,
            ..Default::default()
        }
    }

    fn named(handed: &Handed) -> Vec<String> {
        match handed {
            Handed::Part(part) => part.subjects.iter().map(|s| s.entity_id.clone()).collect(),
            _ => Vec::new(),
        }
    }

    fn subjects(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("INS-{i:05}")).collect()
    }

    #[test]
    fn every_subject_is_kept_by_exactly_one_live_shard_and_the_shares_are_about_even() {
        let live = BTreeSet::from([0, 1, 2]);
        let mut held = [0usize; 3];
        for subject in subjects(3_000) {
            held[keeper(&subject, &live).unwrap() as usize] += 1;
        }
        assert_eq!(held.iter().sum::<usize>(), 3_000);
        for (shard, count) in held.iter().enumerate() {
            assert!(
                (800..=1_200).contains(count),
                "shard {shard} keeps {count} of 3,000"
            );
        }
        assert_eq!(keeper("INS-1", &BTreeSet::new()), None);
    }

    #[test]
    fn a_shard_lost_moves_only_its_own_subjects_and_one_added_takes_about_one_in_n() {
        let three = BTreeSet::from([0, 1, 2]);
        let two = BTreeSet::from([0, 2]);
        let four = BTreeSet::from([0, 1, 2, 3]);
        let mut moved_to_new = 0;
        for subject in subjects(4_000) {
            let before = keeper(&subject, &three).unwrap();
            let after = keeper(&subject, &two).unwrap();
            if before != 1 {
                assert_eq!(before, after, "{subject} moved though its shard lives");
            }
            let grown = keeper(&subject, &four).unwrap();
            if grown != before {
                assert_eq!(grown, 3, "{subject} moved between shards that both live");
                moved_to_new += 1;
            }
        }
        assert!(
            (800..=1_200).contains(&moved_to_new),
            "{moved_to_new} of 4,000 moved to the fourth shard"
        );
    }

    #[test]
    fn the_weights_are_the_same_in_every_release() {
        // Two shards of different releases must agree, so the function is
        // pinned: a change here moves every subject of every instance.
        assert_eq!(weight("INS-00001", 0), 0x36c2_e145_c32e_ffe1);
        assert_eq!(weight("INS-00001", 1), 0xfe8f_7d80_a478_e05f);
    }

    #[test]
    fn two_shards_hand_on_a_want_s_subjects_once_each_between_them() {
        let (a, b) = (Shards::new(0), Shards::new(1));
        a.set_live([1]);
        b.set_live([0]);
        let names = subjects(200);
        let all: Vec<&str> = names.iter().map(String::as_str).collect();
        let at_a = named(&a.heard(want("w1", &all, true)));
        let at_b = named(&b.heard(want("w1", &all, true)));
        assert!(!at_a.is_empty() && !at_b.is_empty());
        let mut both: Vec<String> = at_a.iter().chain(&at_b).cloned().collect();
        both.sort();
        assert_eq!(both, names, "each subject at exactly one shard");
    }

    #[test]
    fn a_want_with_none_of_this_shard_s_subjects_is_not_handed_on() {
        let a = Shards::new(0);
        a.set_live([1]);
        let other = subjects(50)
            .into_iter()
            .find(|s| !a.keeps(s))
            .expect("a subject of the other shard");
        assert_eq!(a.heard(want("w1", &[&other], true)), Handed::Nothing);
        // And its withdrawal is not this shard's plugin's to hear.
        assert!(!a.withdrawn("w1"));
    }

    #[test]
    fn a_lost_shard_s_subjects_are_handed_on_at_once_and_given_back_when_it_returns() {
        let a = Shards::new(0);
        a.set_live([1]);
        let names = subjects(100);
        let all: Vec<&str> = names.iter().map(String::as_str).collect();
        let first = named(&a.heard(want("w1", &all, true)));
        assert!(first.len() < names.len());
        // A want asked once is not shared again: the lake asks it again.
        a.heard(want("once", &all, false));
        assert!(a.shared_again().is_empty(), "nothing changed yet");

        // Shard 1's Lease lapsed: every subject is shard 0's.
        let changes = a.changes();
        a.set_live([]);
        assert!(changes.has_changed().unwrap());
        let again = a.shared_again();
        assert_eq!(again.len(), 1, "the standing want alone: {again:?}");
        assert_eq!(named(&again[0]), names);
        assert!(a.shared_again().is_empty(), "handed on once");

        // It returned: its subjects are its own again, the same as before.
        a.set_live([1]);
        let back = a.shared_again();
        assert_eq!(named(&back[0]), first);
    }

    #[test]
    fn a_want_whose_part_here_became_empty_is_handed_on_as_withdrawn() {
        let a = Shards::new(0);
        let other = {
            let probe = Shards::new(0);
            probe.set_live([1]);
            subjects(50).into_iter().find(|s| !probe.keeps(s)).unwrap()
        };
        // Alone, the subject is this shard's.
        assert_eq!(
            named(&a.heard(want("w1", &[&other], true))),
            vec![other.clone()]
        );
        // Shard 1 came up and keeps it: withdrawn here, once.
        a.set_live([1]);
        assert_eq!(
            a.shared_again(),
            vec![Handed::Withdrawn(WantWithdrawnEvent {
                want_id: "w1".into(),
                dataset: "feed:prices".into()
            })]
        );
        assert!(a.shared_again().is_empty());
        // The lake's redelivery finds nothing here to hand on, nor withdraw.
        assert_eq!(a.heard(want("w1", &[&other], true)), Handed::Nothing);
        assert!(!a.withdrawn("w1"));
    }

    #[test]
    fn a_want_withdrawn_is_handed_on_where_some_of_it_was_here_and_forgotten() {
        let a = Shards::new(0);
        a.heard(want("w1", &["INS-1"], true));
        assert!(a.withdrawn("w1"));
        assert!(!a.withdrawn("w1"), "forgotten");
        a.set_live([1]);
        assert!(a.shared_again().is_empty());
    }

    #[test]
    fn the_oldest_want_is_forgotten_past_the_most_kept() {
        let a = Shards::new(0);
        for i in 0..=WANTS_KEPT {
            a.heard(want(&format!("w{i}"), &["INS-1"], true));
        }
        assert!(!a.withdrawn("w0"), "the oldest was forgotten");
        assert!(a.withdrawn("w1"));
    }

    /// Review finding 8: the live shards change, and every want kept is
    /// shared again, between a want's part being worked out and its being
    /// kept. Whatever the order, the want ends whole: what the lost shard
    /// kept of it is handed on here.
    #[test]
    fn a_want_heard_while_the_live_shards_change_is_still_shared_whole() {
        let all = subjects(64);
        let names: Vec<&str> = all.iter().map(String::as_str).collect();
        let shards = std::sync::Arc::new(Shards::new(0));
        shards.set_live([1]);
        let (told, again) = std::sync::mpsc::channel();
        let racing = std::sync::Arc::clone(&shards);
        *shards.between.lock().unwrap() = Some(Box::new(move || {
            let (racing, told) = (std::sync::Arc::clone(&racing), told.clone());
            let (done, ended) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                // Shard 1 is gone, and the reshare runs.
                racing.set_live([]);
                let _ = told.send(racing.shared_again());
                let _ = done.send(());
            });
            // Ended before the want is kept, where nothing holds it back.
            let _ = ended.recv_timeout(std::time::Duration::from_millis(300));
        }));
        let first = shards.heard(want("w", &names, true));
        *shards.between.lock().unwrap() = None;
        let mut handed: BTreeSet<String> = named(&first).into_iter().collect();
        for more in again
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the reshare ended")
        {
            handed.extend(named(&more));
        }
        assert_eq!(
            handed.len(),
            all.len(),
            "subjects of the want kept by no shard"
        );
    }

    /// Review finding 9: one-off wants, however many, push no standing want
    /// out of those kept to share again.
    #[test]
    fn a_burst_of_one_off_wants_forgets_no_standing_want() {
        let all = subjects(64);
        let names: Vec<&str> = all.iter().map(String::as_str).collect();
        let shards = Shards::new(0);
        shards.set_live([1]);
        let first = named(&shards.heard(want("standing", &names, true)));
        assert!(first.len() < all.len());
        for n in 0..=WANTS_KEPT {
            shards.heard(want(&format!("once-{n}"), &names[..2], false));
        }
        shards.set_live([]);
        let again = shards.shared_again();
        assert_eq!(again.len(), 1, "the standing want is shared again");
        assert_eq!(named(&again[0]).len(), all.len());
    }

    // ---- Ruling 184 (option B): a take holds its instance's wants until
    // its plugin's stream opens. ----

    /// A bus that records what it is asked to send and how it subscribes,
    /// over one in memory.
    #[derive(Default)]
    struct Recording {
        memory: meridian_bus::MemoryBackend,
        sent: Mutex<Vec<String>>,
        shared: Mutex<Vec<String>>,
    }

    impl meridian_bus::Backend for Recording {
        fn publish(
            &self,
            topic: &str,
            envelope: meridian_bus::Envelope,
        ) -> Result<u64, meridian_bus::BusError> {
            self.sent.lock().unwrap().push(topic.to_string());
            self.memory.publish(topic, envelope)
        }
        fn subscribe(&self, pattern: &str) -> Subscription {
            self.memory.subscribe(pattern)
        }
        fn subscribe_shared(&self, pattern: &str, group: &str) -> Subscription {
            self.shared.lock().unwrap().push(pattern.to_string());
            self.memory.subscribe_shared(pattern, group)
        }
        fn dropped(&self) -> u64 {
            self.memory.dropped()
        }
    }

    /// The pod's bus and the lake's, on one broker that records the pod's.
    fn buses() -> (
        std::sync::Arc<Bus>,
        std::sync::Arc<Recording>,
        std::sync::Arc<Bus>,
    ) {
        let recording = std::sync::Arc::new(Recording::default());
        // The pod's bus records; the lake's reaches it through the memory
        // the recording wraps.
        let pod = std::sync::Arc::new(Bus::single(
            "feed",
            std::sync::Arc::clone(&recording) as std::sync::Arc<dyn meridian_bus::Backend>,
            std::sync::Arc::new(meridian_clock::SystemClock),
        ));
        let lake = std::sync::Arc::new(Bus::single(
            "lake",
            std::sync::Arc::new(Forward(std::sync::Arc::clone(&recording))),
            std::sync::Arc::new(meridian_clock::SystemClock),
        ));
        (pod, recording, lake)
    }

    /// The lake's side of the broker: what it sends reaches the pod's
    /// subscriptions, and is not the pod's own sending.
    struct Forward(std::sync::Arc<Recording>);

    impl meridian_bus::Backend for Forward {
        fn publish(
            &self,
            topic: &str,
            envelope: meridian_bus::Envelope,
        ) -> Result<u64, meridian_bus::BusError> {
            self.0.memory.publish(topic, envelope)
        }
        fn subscribe(&self, pattern: &str) -> Subscription {
            self.0.memory.subscribe(pattern)
        }
        fn subscribe_shared(&self, pattern: &str, group: &str) -> Subscription {
            self.0.memory.subscribe_shared(pattern, group)
        }
        fn dropped(&self) -> u64 {
            self.0.memory.dropped()
        }
    }

    fn publish_want(lake: &Bus, want: &ObservationsWantedEvent) {
        lake.publish(
            WANTED_TOPIC,
            "meridian.v1.ObservationsWantedEvent",
            want.encode_to_vec(),
            None,
            None,
        )
        .unwrap();
    }

    fn publish_withdrawn(lake: &Bus, id: &str) {
        lake.publish(
            WITHDRAWN_TOPIC,
            "meridian.v1.WantWithdrawnEvent",
            WantWithdrawnEvent {
                want_id: id.into(),
                dataset: "feed:prices".into(),
            }
            .encode_to_vec(),
            None,
            None,
        )
        .unwrap();
    }

    /// Long enough for the hold to read what was just published.
    async fn settle() {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    fn ids(held: &Held) -> Vec<&str> {
        held.wants
            .iter()
            .map(|(want, _)| want.want_id.as_str())
            .collect()
    }

    const LONG: std::time::Duration = std::time::Duration::from_secs(60);

    /// A want withdrawn in the hold's window is never handed, whichever of
    /// the two was heard first; another instance's wants are not held; the
    /// lake's redelivery of a want held is one want, the latest.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_hold_hands_each_want_once_and_never_one_withdrawn_in_its_window() {
        clear_fenced();
        let (pod, _, lake) = buses();
        let shards = Shards::new(0);
        shards.hold_wants(&pod, "feed", LONG);
        publish_want(&lake, &want("kept", &["INS-1"], true));
        publish_want(&lake, &want("gone", &["INS-2"], true));
        // Held, then withdrawn.
        settle().await;
        publish_withdrawn(&lake, "gone");
        // Its withdrawal heard before it, as two topics may arrive.
        publish_withdrawn(&lake, "late");
        publish_want(&lake, &want("late", &["INS-3"], true));
        publish_want(
            &lake,
            &ObservationsWantedEvent {
                dataset: "other:prices".into(),
                ..want("theirs", &["INS-4"], true)
            },
        );
        // The take's delivery and the sweep's of one want, in one window.
        publish_want(&lake, &want("kept", &["INS-1", "INS-5"], true));
        settle().await;
        let held = shards.take_held().await.expect("the hold answered");
        assert_eq!(ids(&held), vec!["kept"]);
        assert_eq!(held.wants[0].0.subjects.len(), 2, "the latest of the two");
        assert!(!held.lost);
    }

    /// The hold is bounded: past the most it holds, a want is left out and
    /// said lost. It empties at the stream's opening -- asked once, it
    /// holds nothing after -- and ends at its bound unasked.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_hold_is_bounded_and_empties_at_the_stream_s_opening() {
        clear_fenced();
        let (pod, _, lake) = buses();
        let shards = Shards::new(0);
        shards.hold_wants(&pod, "feed", LONG);
        for n in 0..=HOLD_MOST {
            publish_want(&lake, &want(&format!("w{n}"), &["INS-1"], true));
            if n % 200 == 0 {
                settle().await;
            }
        }
        settle().await;
        let held = shards.take_held().await.expect("the hold answered");
        assert_eq!(held.wants.len(), HOLD_MOST);
        assert!(held.lost, "a want left out was not said");
        assert!(
            shards.take_held().await.is_none(),
            "held after its hand-over"
        );

        // Unasked past its bound: it ends, and hands nothing.
        let shards = Shards::new(0);
        shards.hold_wants(&pod, "feed", std::time::Duration::from_millis(200));
        publish_want(&lake, &want("w", &["INS-1"], true));
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert!(shards.take_held().await.is_none(), "held past its bound");
    }

    /// A shard fenced hands nothing of what it held; and a shard that holds
    /// nothing -- its plugin hears no wants, or its serving made a new
    /// keep -- has nothing to hand.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_fenced_shard_hands_nothing_it_held() {
        let (pod, _, lake) = buses();
        clear_fenced();
        let shards = Shards::new(0);
        shards.hold_wants(&pod, "feed", LONG);
        publish_want(&lake, &want("w", &["INS-1"], true));
        settle().await;
        say_fenced("a test's refusal");
        let fenced = shards.take_held().await;
        clear_fenced();
        assert!(fenced.is_none(), "a fenced shard handed what it held");
        // The next serving's keep is new: it holds nothing of the last.
        assert!(Shards::new(0).take_held().await.is_none());
    }

    /// Only a plugin whose roles hear wants (the `data` role) holds them.
    #[test]
    fn only_a_role_that_hears_wants_holds_them() {
        assert!(hears_wants(&["data".to_string()]));
        assert!(hears_wants(&["signal".to_string(), "data".to_string()]));
        for role in [
            "compliance",
            "confirmation",
            "connectivity",
            "custody",
            "ems",
            "oms",
            "operations",
            "portfolio",
            "pricing",
            "reference",
            "reporting",
            "servicing",
            "settlement",
            "signal",
        ] {
            assert!(!hears_wants(&[role.to_string()]), "{role} holds wants");
        }
    }

    /// The hold only listens: it sends nothing, and its subscriptions are
    /// every subscriber's, not a group's, so every other hearer of a want
    /// still hears it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_hold_sends_nothing_and_takes_no_want_from_another_hearer() {
        clear_fenced();
        let (pod, recording, lake) = buses();
        let mut other = lake.subscribe_every(WANTED_TOPIC);
        let shards = Shards::new(0);
        shards.hold_wants(&pod, "feed", LONG);
        publish_want(&lake, &want("w", &["INS-1"], true));
        publish_withdrawn(&lake, "x");
        settle().await;
        let held = shards.take_held().await.expect("the hold answered");
        assert_eq!(ids(&held), vec!["w"]);
        assert!(other.try_recv().is_some(), "another hearer missed the want");
        assert!(
            recording.sent.lock().unwrap().is_empty(),
            "the hold sent {:?}",
            recording.sent.lock().unwrap()
        );
        assert!(recording.shared.lock().unwrap().is_empty());
    }

    // ---- Review c5z: F2 and the hold's bound. ----

    /// Review c5z F2 (b): a want heard after its withdrawal -- two topics,
    /// two forwarders, either order -- is never kept, so never handed, and
    /// not shared again at a change: the lake has forgotten it, and nothing
    /// would withdraw it.
    #[test]
    fn a_want_heard_after_its_withdrawal_is_never_kept_or_handed() {
        let shards = Shards::new(0);
        assert!(!shards.withdrawn("late"), "a want not kept was handed");
        assert_eq!(
            shards.heard(want("late", &["INS-1", "INS-2"], true)),
            Handed::Nothing,
            "a want heard after its withdrawal was handed"
        );
        shards.set_live([1]);
        assert!(
            shards.shared_again().is_empty(),
            "a want heard after its withdrawal was kept"
        );
    }

    /// The withdrawals remembered are bounded as the keep is: past
    /// [`WITHDRAWALS_KEPT`], the oldest is forgotten first.
    #[test]
    fn the_withdrawals_remembered_are_bounded_the_oldest_forgotten_first() {
        let shards = Shards::new(0);
        for n in 0..=WITHDRAWALS_KEPT {
            shards.withdrawn(&format!("gone-{n}"));
        }
        assert_eq!(shards.wants.lock().unwrap().gone.len(), WITHDRAWALS_KEPT);
        assert!(matches!(
            shards.heard(want("gone-0", &["INS-1"], true)),
            Handed::Part(_)
        ));
        assert_eq!(
            shards.heard(want(&format!("gone-{WITHDRAWALS_KEPT}"), &["INS-1"], true)),
            Handed::Nothing
        );
    }

    /// Review c5z F2 (c): the hold read a want's withdrawal, and the want
    /// was still in its subscription when the stream asked. The stream
    /// forwards it from there, and it is never handed.
    #[tokio::test]
    async fn a_want_left_in_the_hold_s_subscription_after_its_withdrawal_is_never_handed() {
        clear_fenced();
        let (pod, _, lake) = buses();
        let shards = Shards::new(0);
        shards.hold_wants(&pod, "feed", LONG);
        publish_withdrawn(&lake, "w");
        // The hold reads the withdrawal.
        settle().await;
        // The want waits in the hold's subscription: on this one thread the
        // hold does not run before the stream's ask, which it reads first.
        publish_want(&lake, &want("w", &["INS-1", "INS-2"], true));
        let held = shards.take_held().await.expect("the hold answered");
        assert!(held.wants.is_empty());
        let mut wanted = held.wanted;
        let heard = wanted.recv().await.expect("the want, still waiting");
        let heard = ObservationsWantedEvent::decode(&heard.envelope.payload[..]).unwrap();
        assert_eq!(heard.want_id, "w");
        assert_eq!(
            shards.heard(heard),
            Handed::Nothing,
            "a want withdrawn in the hold was handed after it"
        );
    }

    /// The hold's bound counts from the take's answer, not from the hold's
    /// start (review c5z, a nit): the lake's redelivery of what the take
    /// delivered again falls within it.
    #[tokio::test(start_paused = true)]
    async fn the_hold_s_bound_counts_from_the_take_s_answer() {
        clear_fenced();
        let (pod, _, lake) = buses();
        let shards = Shards::new(0);
        let bound = std::time::Duration::from_secs(75);
        shards.hold_wants(&pod, "feed", bound);
        // The take answered a while after the hold began.
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        shards.hold_from_the_take();
        // Past the bound from the hold's start, within it from the take's.
        tokio::time::sleep(bound - std::time::Duration::from_secs(2)).await;
        publish_want(&lake, &want("w", &["INS-1"], true));
        settle().await;
        let held = shards
            .take_held()
            .await
            .expect("the hold ended before its bound from the take");
        assert_eq!(ids(&held), vec!["w"]);
    }
}
