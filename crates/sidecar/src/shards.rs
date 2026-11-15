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

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::{Mutex, RwLock};

use meridian_domain::v1::{ObservationsWantedEvent, WantWithdrawnEvent};
use tokio::sync::watch;

/// The most wants a shard keeps to share again when the live shards change:
/// the oldest forgotten first, and shared again at the lake's redelivery.
const WANTS_KEPT: usize = 4_096;

/// One shard's view of its instance's shards.
pub struct Shards {
    /// The shard this sidecar is: the `n` of the Lease it holds.
    mine: u32,
    live: RwLock<BTreeSet<u32>>,
    changed: watch::Sender<u64>,
    wants: Mutex<Wants>,
}

#[derive(Default)]
struct Wants {
    /// Each want heard, whole, and the subjects of it handed to the plugin.
    kept: HashMap<String, (ObservationsWantedEvent, BTreeSet<String>)>,
    order: VecDeque<String>,
}

/// What a shard's plugin is handed of a want heard.
#[derive(Debug, Clone, PartialEq)]
pub enum Handed {
    /// The want, naming this shard's subjects alone.
    Part(ObservationsWantedEvent),
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
            live: RwLock::new(BTreeSet::from([mine])),
            changed: watch::channel(0).0,
            wants: Mutex::default(),
        }
    }

    pub fn mine(&self) -> u32 {
        self.mine
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

    /// A want heard: kept whole, and its part here to hand on.
    pub fn heard(&self, want: ObservationsWantedEvent) -> Handed {
        let (part, subjects) = self.part(&want);
        let mut wants = self.wants.lock().expect("wants poisoned");
        let before = wants
            .kept
            .get(&want.want_id)
            .map(|(_, handed)| !handed.is_empty());
        if before.is_none() {
            wants.order.push_back(want.want_id.clone());
            while wants.order.len() > WANTS_KEPT {
                if let Some(oldest) = wants.order.pop_front() {
                    wants.kept.remove(&oldest);
                }
            }
        }
        let (want_id, dataset) = (want.want_id.clone(), want.dataset.clone());
        wants.kept.insert(want_id.clone(), (want, subjects.clone()));
        if !subjects.is_empty() {
            Handed::Part(part)
        } else if before == Some(true) {
            Handed::Withdrawn(WantWithdrawnEvent { want_id, dataset })
        } else {
            Handed::Nothing
        }
    }

    /// A want the lake withdrew: forgotten, and handed on only where some
    /// of it was this shard's.
    pub fn withdrawn(&self, want_id: &str) -> bool {
        let mut wants = self.wants.lock().expect("wants poisoned");
        wants.order.retain(|held| held != want_id);
        wants
            .kept
            .remove(want_id)
            .is_some_and(|(_, handed)| !handed.is_empty())
    }

    /// The standing wants whose part here changed since each was last
    /// handed on, shared again over the live shards as they are now.
    pub fn shared_again(&self) -> Vec<Handed> {
        let mut again = Vec::new();
        let mut wants = self.wants.lock().expect("wants poisoned");
        let ids: Vec<String> = wants.order.iter().cloned().collect();
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
            if let Some(kept) = wants.kept.get_mut(&id) {
                kept.1 = subjects.clone();
            }
            again.push(if subjects.is_empty() {
                Handed::Withdrawn(WantWithdrawnEvent {
                    want_id: id,
                    dataset,
                })
            } else {
                Handed::Part(part)
            });
        }
        again
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
}
