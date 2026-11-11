//! Never blind (contract v30, W10.6; the resilience spec's section 8; rule
//! 1050): each switch of a default read's source, and each switch back, is
//! its own record, `SourceSwitched`, journalled by the lake in its `control`
//! partition and read by `ListSourceSwitches`.
//!
//! A default read of the latest now passes over a source that is silent
//! beyond its cadence, stalled -- no sign of life within its stale ceiling --
//! or quarantined, its latest rows held invalid, for the next by the
//! deployment's priority ([`crate::read`]). What each such read served and
//! passed over is held here against what the lake last recorded, so a switch
//! is recorded once, when a default read first serves past the source, and
//! a switch back once, when one first serves from it again:
//!
//! - **stalled** is the source's whole: its record names no subjects, every
//!   subject the priority serves;
//! - **silent** and **quarantined** are held per subject, as the rows they
//!   are judged by are: a record names the subjects it is for, at most 500,
//!   one record per 500.
//!
//! What is held is rebuilt from the records at start ([`Switches::replay`]),
//! so a writer taking over does not record a switch again. Only the
//! writer records: a reader member never writes.

use std::collections::BTreeMap;

use meridian_domain::v1::{SourceSwitchReason, SourceSwitched};

use crate::read::Switching;
use crate::row::DataType;

/// The most subjects one record names (its dictionary's bound).
pub const SUBJECTS_MOST: usize = 500;

/// The most passed-over sources held at once, across every priority and
/// subject: past it a new switch is still recorded, and its switch back is
/// not, which the lake says.
pub const HELD_MOST: usize = 50_000;

/// A source passed over: its priority's data type and kind, the dataset, and
/// the subject -- empty where the whole source was (stalled).
type Key = (String, i32, String, String);

/// What a passed-over source was passed over for, and what served instead.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Passed {
    to: String,
    reason: i32,
}

/// The sources passed over now, as the lake last recorded them.
#[derive(Debug, Default)]
pub struct Switches {
    held: BTreeMap<Key, Passed>,
    /// Whether the bound turned a switch away since the lake last said so.
    over: bool,
}

impl Switches {
    /// What the records say is passed over now: each switch held, each
    /// switch back letting it go, in the order recorded.
    pub fn replay(&mut self, recorded: &[SourceSwitched]) {
        for switch in recorded {
            let whole = switch.subjects.is_empty();
            let subjects: Vec<String> = if whole {
                vec![String::new()]
            } else {
                switch.subjects.clone()
            };
            if switch.reason == SourceSwitchReason::Restored as i32 {
                for subject in &subjects {
                    self.held.remove(&(
                        switch.data_type.clone(),
                        switch.kind,
                        switch.to_dataset.clone(),
                        subject.clone(),
                    ));
                }
                // A source restored whole lets go of its subjects too.
                if whole {
                    self.held.retain(|(data_type, kind, from, _), _| {
                        !(data_type == &switch.data_type
                            && *kind == switch.kind
                            && from == &switch.to_dataset)
                    });
                }
            } else {
                for subject in subjects {
                    self.held.insert(
                        (
                            switch.data_type.clone(),
                            switch.kind,
                            switch.from_dataset.clone(),
                            subject,
                        ),
                        Passed {
                            to: switch.to_dataset.clone(),
                            reason: switch.reason,
                        },
                    );
                }
            }
        }
    }

    /// The switches a default read's answer makes, to be recorded: each
    /// source it passed over that was not already, and each it served from
    /// that was passed over. Held as recorded from here; a record that does
    /// not commit is let go again by [`Switches::forget`].
    pub fn observe(
        &mut self,
        data_type: DataType,
        switching: &[Switching],
        now_ns: i64,
    ) -> Vec<SourceSwitched> {
        let name = data_type.name().to_string();
        // Only a price's priority names a kind (a rate's names none, W10.3).
        let kind_of = |kind: i32| {
            if data_type == DataType::Price {
                kind
            } else {
                0
            }
        };
        // By what each record says but its subjects, so a read of many
        // subjects records one switch for them all.
        let mut made: BTreeMap<(i32, String, String, i32), Vec<String>> = BTreeMap::new();
        for seen in switching {
            let kind = kind_of(seen.kind);
            for (from, reason) in &seen.passed {
                let subject = if *reason == SourceSwitchReason::Stalled {
                    String::new()
                } else {
                    seen.subject.clone()
                };
                let key = (name.clone(), kind, from.clone(), subject.clone());
                // Passed over already, or the whole source stalled already.
                if self.held.contains_key(&key)
                    || self
                        .held
                        .contains_key(&(name.clone(), kind, from.clone(), String::new()))
                {
                    continue;
                }
                if self.held.len() >= HELD_MOST {
                    self.over = true;
                    continue;
                }
                self.held.insert(
                    key,
                    Passed {
                        to: seen.served.clone(),
                        reason: *reason as i32,
                    },
                );
                made.entry((kind, from.clone(), seen.served.clone(), *reason as i32))
                    .or_default()
                    .push(subject);
            }
            if seen.served.is_empty() {
                continue;
            }
            // Served from again: switched back, for the subject, or for the
            // whole source where it stalled.
            for subject in [seen.subject.clone(), String::new()] {
                let key = (name.clone(), kind, seen.served.clone(), subject.clone());
                if let Some(passed) = self.held.remove(&key) {
                    // The one it leaves; itself where none was left and its
                    // own last good rows stood.
                    let from = if passed.to.is_empty() {
                        seen.served.clone()
                    } else {
                        passed.to
                    };
                    made.entry((
                        kind,
                        from,
                        seen.served.clone(),
                        SourceSwitchReason::Restored as i32,
                    ))
                    .or_default()
                    .push(subject);
                }
            }
        }
        let mut out = Vec::new();
        for ((kind, from, to, reason), mut subjects) in made {
            subjects.sort();
            subjects.dedup();
            // The whole source: no subjects named.
            let whole = subjects.iter().any(String::is_empty);
            let chunks: Vec<Vec<String>> = if whole {
                vec![Vec::new()]
            } else {
                subjects
                    .chunks(SUBJECTS_MOST)
                    .map(<[String]>::to_vec)
                    .collect()
            };
            for subjects in chunks {
                out.push(SourceSwitched {
                    data_type: name.clone(),
                    kind,
                    from_dataset: from.clone(),
                    to_dataset: to.clone(),
                    subjects,
                    reason,
                    switched_at_ns: now_ns,
                });
            }
        }
        out
    }

    /// Records that did not commit: what they held is let go, so the next
    /// default read records them again.
    pub fn forget(&mut self, unrecorded: &[SourceSwitched]) {
        for switch in unrecorded {
            if switch.reason == SourceSwitchReason::Restored as i32 {
                continue;
            }
            let subjects: Vec<String> = if switch.subjects.is_empty() {
                vec![String::new()]
            } else {
                switch.subjects.clone()
            };
            for subject in subjects {
                self.held.remove(&(
                    switch.data_type.clone(),
                    switch.kind,
                    switch.from_dataset.clone(),
                    subject,
                ));
            }
        }
    }

    /// Whether the bound turned a switch away since this was last asked.
    pub fn over_bound(&mut self) -> bool {
        std::mem::take(&mut self.over)
    }

    /// How many passed-over sources are held.
    pub fn len(&self) -> usize {
        self.held.len()
    }

    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(subject: &str, served: &str, passed: &[(&str, SourceSwitchReason)]) -> Switching {
        Switching {
            kind: 0,
            subject: subject.into(),
            served: served.into(),
            passed: passed.iter().map(|(d, r)| (d.to_string(), *r)).collect(),
        }
    }

    #[test]
    fn a_switch_is_recorded_once_and_its_switch_back_once() {
        let mut held = Switches::default();
        let first = held.observe(
            DataType::Quote,
            &[seen(
                "ENT-A",
                "b:quotes",
                &[("a:quotes", SourceSwitchReason::Stalled)],
            )],
            10,
        );
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].from_dataset, "a:quotes");
        assert_eq!(first[0].to_dataset, "b:quotes");
        assert_eq!(first[0].reason, SourceSwitchReason::Stalled as i32);
        assert!(
            first[0].subjects.is_empty(),
            "a stall is the whole source's"
        );
        // Passed over again, for another subject too: nothing new.
        let again = held.observe(
            DataType::Quote,
            &[
                seen(
                    "ENT-A",
                    "b:quotes",
                    &[("a:quotes", SourceSwitchReason::Stalled)],
                ),
                seen(
                    "ENT-B",
                    "b:quotes",
                    &[("a:quotes", SourceSwitchReason::Silent)],
                ),
            ],
            11,
        );
        assert!(again.is_empty(), "{again:?}");
        // Served from again: one switch back, the whole source's.
        let back = held.observe(DataType::Quote, &[seen("ENT-A", "a:quotes", &[])], 12);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].reason, SourceSwitchReason::Restored as i32);
        assert_eq!(
            (back[0].from_dataset.as_str(), back[0].to_dataset.as_str()),
            ("b:quotes", "a:quotes")
        );
        assert!(back[0].subjects.is_empty());
        assert!(held.is_empty());
        assert!(held
            .observe(DataType::Quote, &[seen("ENT-A", "a:quotes", &[])], 13)
            .is_empty());
    }

    #[test]
    fn a_quarantine_is_held_per_subject_and_one_record_names_them_all() {
        let mut held = Switches::default();
        let made = held.observe(
            DataType::Quote,
            &[
                seen(
                    "ENT-B",
                    "b:quotes",
                    &[("a:quotes", SourceSwitchReason::Quarantined)],
                ),
                seen(
                    "ENT-A",
                    "b:quotes",
                    &[("a:quotes", SourceSwitchReason::Quarantined)],
                ),
            ],
            10,
        );
        assert_eq!(made.len(), 1);
        assert_eq!(
            made[0].subjects,
            vec!["ENT-A".to_string(), "ENT-B".to_string()]
        );
        // One subject's latest good again: its switch back alone.
        let back = held.observe(DataType::Quote, &[seen("ENT-A", "a:quotes", &[])], 11);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].subjects, vec!["ENT-A".to_string()]);
        assert_eq!(held.len(), 1);
    }

    #[test]
    fn what_is_held_is_rebuilt_from_the_records_so_a_takeover_records_nothing_again() {
        let mut writer = Switches::default();
        let made = writer.observe(
            DataType::Quote,
            &[seen(
                "ENT-A",
                "",
                &[("a:quotes", SourceSwitchReason::Stalled)],
            )],
            10,
        );
        assert_eq!(made[0].to_dataset, "", "none left to serve");
        let mut next = Switches::default();
        next.replay(&made);
        assert!(next
            .observe(
                DataType::Quote,
                &[seen(
                    "ENT-A",
                    "",
                    &[("a:quotes", SourceSwitchReason::Stalled)]
                )],
                11
            )
            .is_empty());
        let back = next.observe(DataType::Quote, &[seen("ENT-A", "a:quotes", &[])], 12);
        let mut after = Switches::default();
        after.replay(&[made, back].concat());
        assert!(after.is_empty());
    }

    #[test]
    fn a_record_that_did_not_commit_is_made_again() {
        let mut held = Switches::default();
        let made = held.observe(
            DataType::Quote,
            &[seen(
                "ENT-A",
                "b:quotes",
                &[("a:quotes", SourceSwitchReason::Silent)],
            )],
            10,
        );
        held.forget(&made);
        assert_eq!(
            held.observe(
                DataType::Quote,
                &[seen(
                    "ENT-A",
                    "b:quotes",
                    &[("a:quotes", SourceSwitchReason::Silent)]
                )],
                11
            )
            .len(),
            1
        );
    }
}
